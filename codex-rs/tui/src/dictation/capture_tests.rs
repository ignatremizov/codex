use super::*;
use pretty_assertions::assert_eq;

fn control() -> Arc<Control> {
    Arc::new(Control {
        stop: CancellationToken::new(),
        cancel: CancellationToken::new(),
        failed: AtomicBool::new(/*v*/ false),
        pressured: AtomicBool::new(/*v*/ false),
        peak: Arc::new(AtomicU16::new(/*v*/ 0)),
    })
}

fn block(samples: &[i16]) -> Block {
    let mut block = Block {
        samples: [0; BLOCK_SAMPLES],
        len: samples.len(),
    };
    block.samples[..samples.len()].copy_from_slice(samples);
    block
}

async fn receive(rx: &mut mpsc::Receiver<CaptureEvent>) -> CaptureEvent {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), rx.recv())
        .await
        .expect("capture event deadline")
        .expect("capture event")
}

async fn joined(worker: thread::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        while !worker.is_finished() {
            tokio::time::sleep(SERVICE_INTERVAL).await;
        }
    })
    .await
    .expect("capture thread deadline");
    worker.join().expect("capture thread");
}

#[tokio::test]
async fn stop_drains_accepted_audio_and_subframe_tail() {
    let control = control();
    let slots = Arc::new(Semaphore::new(/*permits*/ 1));
    let permit = slots.clone().try_acquire_owned().unwrap();
    let (input, output) = ingress::sync_channel(/*bound*/ 2);
    let samples = (0..337).collect::<Vec<i16>>();
    assert!(enqueue(&input, block(&samples[..320]), &control));
    assert!(enqueue(&input, block(&samples[320..]), &control));
    control.stop.cancel();
    assert!(!enqueue(&input, block(&[999]), &control));
    drop(input);
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 1);
    let worker = thread::spawn(move || {
        chunk_worker(
            output, /*sample_rate*/ 16_000, control, tx, slots, permit,
        );
    });
    let CaptureEvent::Chunk { audio, .. } = receive(&mut rx).await else {
        panic!("expected final partial audio");
    };
    assert_eq!(
        (audio.data, audio.sample_rate, audio.channels),
        (samples, 16_000, 1)
    );
    joined(worker).await;
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn cancelled_capture_discards_pending_audio_and_releases_chunk_reservation() {
    let control = control();
    let slots = Arc::new(Semaphore::new(/*permits*/ 1));
    let permit = slots.clone().try_acquire_owned().unwrap();
    let (input, output) = ingress::sync_channel(/*bound*/ 1);
    assert!(enqueue(&input, block(&[12, 34]), &control));
    control.cancel.cancel();
    drop(input);
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 1);
    let worker_slots = slots.clone();
    let worker = thread::spawn(move || {
        chunk_worker(
            output,
            /*sample_rate*/ 16_000,
            control,
            tx,
            worker_slots,
            permit,
        );
    });
    joined(worker).await;
    assert!(rx.recv().await.is_none());
    assert_eq!(slots.available_permits(), 1);
}

#[tokio::test]
async fn overflow_is_partial_failure_and_preserves_accepted_samples() {
    let control = control();
    let slots = Arc::new(Semaphore::new(/*permits*/ 1));
    let permit = slots.clone().try_acquire_owned().unwrap();
    let (input, output) = ingress::sync_channel(/*bound*/ 1);
    assert!(enqueue(&input, block(&[1, 2, 3]), &control));
    assert!(!enqueue(&input, block(&[4, 5]), &control));
    assert!(control.stop.is_cancelled());
    drop(input);
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 1);
    let worker = thread::spawn(move || {
        chunk_worker(
            output, /*sample_rate*/ 16_000, control, tx, slots, permit,
        );
    });
    let CaptureEvent::Chunk { audio, .. } = receive(&mut rx).await else {
        panic!("expected accepted audio");
    };
    assert_eq!(audio.data, vec![1, 2, 3]);
    assert!(matches!(receive(&mut rx).await, CaptureEvent::Error(_)));
    joined(worker).await;
}

#[tokio::test]
async fn output_backpressure_stops_intake_and_cancel_unblocks_publisher() {
    let control = control();
    let slots = Arc::new(Semaphore::new(/*permits*/ 1));
    let permit = slots.clone().try_acquire_owned().unwrap();
    let (input, output) = ingress::sync_channel(/*bound*/ 1);
    assert!(enqueue(&input, block(&[1, 2, 3]), &control));
    drop(input);
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 1);
    assert!(tx.try_send(CaptureEvent::Finished).is_ok());
    let worker_control = control.clone();
    let worker = thread::spawn(move || {
        chunk_worker(
            output,
            /*sample_rate*/ 16_000,
            worker_control,
            tx,
            slots,
            permit,
        );
    });
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), control.stop.cancelled())
        .await
        .expect("backpressure must stop intake");
    control.cancel.cancel();
    joined(worker).await;
    assert!(matches!(receive(&mut rx).await, CaptureEvent::Finished));
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn lease_is_held_until_actual_owner_exit_even_after_cancelled_startup() {
    let microphone = Arc::new(Semaphore::new(/*permits*/ 1));
    let lease = Arc::new(microphone.clone().try_acquire_owned().unwrap());
    let control = control();
    let terminated = CancellationToken::new();
    let (release, blocked_startup) = ingress::sync_channel(/*bound*/ 1);
    let owner_lease = lease.clone();
    let owner = thread::spawn(move || {
        let _lease = owner_lease;
        blocked_startup
            .recv()
            .expect("release fake device discovery");
    });
    let (tx, _rx) = mpsc::channel(/*buffer*/ 1);
    let monitor = tokio::spawn(monitor_termination(
        owner,
        lease,
        terminated.clone(),
        control.clone(),
        tx,
    ));
    control.cancel.cancel();
    assert_eq!(microphone.available_permits(), 0);
    assert!(!terminated.is_cancelled());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), terminated.cancelled())
        .await
        .expect("actual thread termination");
    monitor.await.unwrap();
    assert_eq!(microphone.available_permits(), 1);
}

#[tokio::test]
async fn failed_owner_reports_error_and_acknowledges_termination() {
    let microphone = Arc::new(Semaphore::new(/*permits*/ 1));
    let lease = Arc::new(microphone.clone().try_acquire_owned().unwrap());
    let control = control();
    let terminated = CancellationToken::new();
    let owner = thread::spawn(|| panic!("synthetic device startup failure"));
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 1);
    let monitor = tokio::spawn(monitor_termination(
        owner,
        lease,
        terminated.clone(),
        control,
        tx,
    ));
    assert!(matches!(receive(&mut rx).await, CaptureEvent::Error(_)));
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), monitor)
        .await
        .expect("failed owner acknowledgment")
        .unwrap();
    assert!(terminated.is_cancelled());
    assert_eq!(microphone.available_permits(), 1);
    assert!(rx.recv().await.is_none());
}

#[test]
fn resampling_keeps_native_pcm_and_exact_frame_count_across_callback_boundaries() {
    for sample_rate in [8_000, 16_000, 24_000, 44_100, 48_000] {
        let mut chunker = Chunker::new(sample_rate);
        let samples = (0..sample_rate + 7)
            .map(|index| (index % 1000) as i16)
            .collect::<Vec<_>>();
        for callback in samples.chunks(/*chunk_size*/ 137) {
            for sample in callback {
                assert!(!chunker.push(*sample));
            }
        }
        assert_eq!(
            chunker.frame.len(),
            ((samples.len() as u64 * 16_000 / u64::from(sample_rate)) % 320) as usize,
        );
        let audio = chunker.take();
        assert_eq!(
            (audio.data, audio.sample_rate, audio.channels),
            (samples, sample_rate, 1)
        );
    }
}

#[test]
fn silence_boundary_and_final_tail_preserve_every_native_sample() {
    let sample_rate = 44_100;
    let first_chunk = sample_rate as usize * 15;
    let mut chunker = Chunker::new(sample_rate);
    let mut lengths = Vec::new();
    for _ in 0..first_chunk + 17 {
        if chunker.push(/*sample*/ 0) {
            lengths.push(chunker.take().data.len());
        }
    }
    lengths.push(chunker.take().data.len());
    assert_eq!(lengths, vec![first_chunk, 17]);
}

#[tokio::test]
async fn capacity_stop_waits_for_ordered_reservation_release_and_flushes_final_tail() {
    let control = control();
    let slots = Arc::new(Semaphore::new(/*permits*/ 1));
    let permit = slots.clone().try_acquire_owned().unwrap();
    let (input, output) = ingress::sync_channel(INGRESS_BLOCKS);
    // Low synthetic rate fits a full silence boundary and tail in bounded ingress.
    let samples = vec![0; 15_017];
    for samples in samples.chunks(BLOCK_SAMPLES) {
        assert!(enqueue(&input, block(samples), &control));
    }
    drop(input);
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 1);
    let worker_control = control.clone();
    let worker = thread::spawn(move || {
        chunk_worker(
            output,
            /*sample_rate*/ 1000,
            worker_control,
            tx,
            slots,
            permit,
        );
    });
    let CaptureEvent::Chunk { audio, reservation } = receive(&mut rx).await else {
        panic!("expected first chunk");
    };
    assert_eq!(audio.data.len(), 15_000);
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), control.stop.cancelled())
        .await
        .expect("capacity must stop intake");
    assert!(matches!(receive(&mut rx).await, CaptureEvent::Error(_)));
    // A completed out-of-order upload still holds this permit until ordered delivery.
    drop(reservation);
    let CaptureEvent::Chunk { audio, .. } = receive(&mut rx).await else {
        panic!("expected final reserved tail");
    };
    assert_eq!(
        (audio.data, audio.sample_rate, audio.channels),
        (vec![0; 17], 1000, 1)
    );
    joined(worker).await;
}

//! Generation-bound projection shared by the goal owner and prompt contributors.

use std::sync::Mutex;
use std::sync::PoisonError;

#[derive(Debug, Default)]
struct Projection {
    generation: u64,
    enabled: bool,
    exhausted: bool,
    objective: Option<String>,
}

/// Thread-scoped projection of goal intent, not a goal store or activation authority.
///
/// The goal owner serializes database mutations separately. All projection changes,
/// including synchronous configuration invalidation, use this one lock.
#[derive(Debug, Default)]
pub struct ActiveGoalObjective {
    projection: Mutex<Projection>,
}

impl ActiveGoalObjective {
    pub fn enabled(&self) -> bool {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .enabled
    }

    pub fn generation(&self) -> u64 {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .generation
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<u64, &'static str> {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if projection.exhausted {
            return Err("goal projection generation exhausted");
        }
        if projection.enabled != enabled {
            advance(&mut projection)?;
            projection.enabled = enabled;
            if !enabled {
                projection.objective = None;
            }
        }
        Ok(projection.generation)
    }

    /// Invalidates outstanding writers while preserving the running turn's projection.
    pub fn advance_generation(&self) -> Result<u64, &'static str> {
        advance(
            &mut self
                .projection
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    pub fn project_if_current(&self, generation: u64, objective: String) -> bool {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if projection.exhausted || !projection.enabled || projection.generation != generation {
            return false;
        }
        projection.objective = Some(objective);
        true
    }

    pub fn clear_if_current(&self, generation: u64) -> bool {
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if projection.exhausted || projection.generation != generation {
            return false;
        }
        projection.objective = None;
        true
    }

    pub fn snapshot(&self) -> Option<String> {
        self.projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .objective
            .clone()
    }
}

fn advance(projection: &mut Projection) -> Result<u64, &'static str> {
    if !projection.exhausted
        && let Some(generation) = projection.generation.checked_add(1)
    {
        projection.generation = generation;
        return Ok(generation);
    }
    projection.exhausted = true;
    projection.enabled = false;
    projection.objective = None;
    Err("goal projection generation exhausted")
}

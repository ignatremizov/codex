# Subagent model overrides

An explicit `spawn_agent` model override must name a model in the loaded catalog. The model's
default multi-agent catalog tag is not a compatibility restriction: a V2 parent can select a
known V1-tagged, Disabled-tagged, or untagged model for a V2 child. Models hidden from the picker
remain valid explicit overrides.

The child's resolved runtime multi-agent version controls its child-management tools and tool
namespace. Choosing a different model does not silently remove V2 delegation tools. Existing
feature enablement and V1 depth limits remain unchanged.

For readability, the tool description lists at most five picker-visible model overrides.
That display limit does not restrict accepted model names. An unknown model is rejected before
creating a child, and its error lists the complete loaded catalog.

Existing role, history-inheritance, reasoning-effort, and service-tier precedence and validation
still apply. This change does not change delegation-mode instructions or authorize a broader
history/role combination.

When a role specifies `model_instructions_file`, the file path is resolved relative to the role
configuration file (or may be absolute). Its non-empty contents replace inherited base
instructions for that child and are marked as custom; developer instructions remain a separate
layer. Missing, unreadable, empty, or invalid-text files identify the selected role and resolved
path in the error, without partially applying the role.

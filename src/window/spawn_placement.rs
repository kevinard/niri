//! Placements requested for the first window of a spawned command.
//!
//! A placement is bound to the xdg-activation token that niri hands to the spawned command. The
//! first toplevel that presents this token before mapping opens according to the placement.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use niri_config::PresetSize;
use niri_ipc::{ColumnDisplay, SizeChange};
use smithay::wayland::xdg_activation::XdgActivationToken;

use super::ResolvedWindowRules;
use crate::layout::workspace::WorkspaceId;

/// How long a placement waits for its window.
///
/// This is longer than the activation token timeout because heavy programs can take a while to
/// show their first window. A token older than the activation timeout still places the window,
/// but no longer focuses it.
pub const SPAWN_PLACEMENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of placements waiting for their windows at once.
///
/// When exceeded, the oldest placement is dropped.
pub const MAX_PENDING_SPAWN_PLACEMENTS: usize = 32;

/// Placement for the first window of a spawned command.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnPlacement {
    /// Workspace to open on.
    ///
    /// Resolved when the command was spawned. `None` if no workspace was requested or the
    /// requested one did not exist.
    pub workspace: Option<WorkspaceId>,
    /// Index of the new column, starting from 0.
    pub column_idx: Option<usize>,
    /// Initial column or floating window width.
    pub width: Option<PresetSize>,
    /// Display mode of the new column.
    pub display: Option<ColumnDisplay>,
    /// Whether to open floating.
    pub floating: Option<bool>,
}

impl SpawnPlacement {
    /// Converts a validated IPC placement with an already resolved workspace.
    pub fn new(placement: niri_ipc::Placement, workspace: Option<WorkspaceId>) -> Self {
        let width = placement.width.and_then(|width| match width {
            SizeChange::SetFixed(fixed) => Some(PresetSize::Fixed(fixed)),
            SizeChange::SetProportion(percent) => Some(PresetSize::Proportion(percent / 100.)),
            // Rejected by validation.
            SizeChange::AdjustFixed(_) | SizeChange::AdjustProportion(_) => None,
        });

        Self {
            workspace,
            column_idx: placement.column_index.map(|idx| idx.saturating_sub(1)),
            width,
            display: placement.display,
            floating: placement.floating,
        }
    }

    /// Overrides the open-time window rules for the fields that this placement sets.
    pub fn apply_to_rules(&self, rules: &mut ResolvedWindowRules) {
        if let Some(width) = self.width {
            rules.default_width = Some(Some(width));
            // An explicit width replaces a full-width column from open-maximized.
            rules.open_maximized = Some(false);
        }
        if let Some(display) = self.display {
            rules.default_column_display = Some(display);
        }
        if let Some(floating) = self.floating {
            rules.open_floating = Some(floating);
        }
    }
}

#[derive(Debug)]
struct PendingSpawnPlacement {
    token: XdgActivationToken,
    created: Instant,
    placement: SpawnPlacement,
}

impl PendingSpawnPlacement {
    fn is_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created) >= SPAWN_PLACEMENT_TIMEOUT
    }
}

/// Placements waiting for the first window that presents their activation token.
#[derive(Debug, Default)]
pub struct SpawnPlacements {
    pending: VecDeque<PendingSpawnPlacement>,
}

impl SpawnPlacements {
    /// Binds a placement to an activation token.
    pub fn insert(&mut self, token: XdgActivationToken, placement: SpawnPlacement, now: Instant) {
        self.remove_expired(now);

        while self.pending.len() >= MAX_PENDING_SPAWN_PLACEMENTS {
            let dropped = self.pending.pop_front().unwrap();
            debug!(
                "dropping the oldest spawn placement for token {}",
                dropped.token.as_str()
            );
        }

        self.pending.push_back(PendingSpawnPlacement {
            token,
            created: now,
            placement,
        });
    }

    /// Removes and returns the placement bound to this token, unless it has expired.
    pub fn take(&mut self, token: &XdgActivationToken, now: Instant) -> Option<SpawnPlacement> {
        let idx = self.pending.iter().position(|p| p.token == *token)?;
        let pending = self.pending.remove(idx)?;
        if pending.is_expired(now) {
            debug!("spawn placement for token {} expired", token.as_str());
            return None;
        }
        Some(pending.placement)
    }

    /// Returns whether a placement that has not expired is bound to this token.
    pub fn contains(&self, token: &XdgActivationToken, now: Instant) -> bool {
        self.pending
            .iter()
            .any(|p| p.token == *token && !p.is_expired(now))
    }

    /// Drops expired placements.
    pub fn remove_expired(&mut self, now: Instant) {
        self.pending.retain(|p| !p.is_expired(now));
    }

    /// Number of placements currently stored.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether no placements are stored.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_token(name: &str) -> XdgActivationToken {
        XdgActivationToken::from(String::from(name))
    }

    fn placement(column_index: usize) -> SpawnPlacement {
        SpawnPlacement::new(
            niri_ipc::Placement {
                column_index: Some(column_index),
                ..Default::default()
            },
            None,
        )
    }

    #[test]
    fn converts_ipc_placement() {
        let placement = SpawnPlacement::new(
            niri_ipc::Placement {
                workspace: Some(niri_ipc::WorkspaceReferenceArg::Name(String::from("dev"))),
                column_index: Some(1),
                width: Some(SizeChange::SetProportion(25.)),
                display: Some(ColumnDisplay::Tabbed),
                floating: Some(false),
            },
            None,
        );
        assert_eq!(placement.column_idx, Some(0));
        assert_eq!(placement.width, Some(PresetSize::Proportion(0.25)));
        assert_eq!(placement.display, Some(ColumnDisplay::Tabbed));
        assert_eq!(placement.floating, Some(false));

        let placement = SpawnPlacement::new(
            niri_ipc::Placement {
                width: Some(SizeChange::SetFixed(800)),
                ..Default::default()
            },
            None,
        );
        assert_eq!(placement.column_idx, None);
        assert_eq!(placement.width, Some(PresetSize::Fixed(800)));
    }

    #[test]
    fn overrides_only_given_rules() {
        let mut rules = ResolvedWindowRules {
            default_width: Some(Some(PresetSize::Proportion(0.5))),
            open_maximized: Some(true),
            default_column_display: Some(ColumnDisplay::Tabbed),
            open_floating: Some(true),
            ..Default::default()
        };

        let mut copy = rules.clone();
        placement(1).apply_to_rules(&mut copy);
        assert_eq!(copy, rules);

        SpawnPlacement {
            workspace: None,
            column_idx: None,
            width: Some(PresetSize::Fixed(640)),
            display: Some(ColumnDisplay::Normal),
            floating: Some(false),
        }
        .apply_to_rules(&mut rules);
        assert_eq!(rules.default_width, Some(Some(PresetSize::Fixed(640))));
        assert_eq!(rules.open_maximized, Some(false));
        assert_eq!(rules.default_column_display, Some(ColumnDisplay::Normal));
        assert_eq!(rules.open_floating, Some(false));
    }

    #[test]
    fn take_is_one_shot() {
        let now = Instant::now();
        let mut placements = SpawnPlacements::default();
        let token = make_token("a");
        let other = make_token("b");
        placements.insert(token.clone(), placement(2), now);

        assert!(placements.contains(&token, now));
        assert!(!placements.contains(&other, now));
        assert_eq!(placements.take(&other, now), None);
        assert_eq!(placements.take(&token, now), Some(placement(2)));
        assert_eq!(placements.take(&token, now), None);
        assert!(placements.is_empty());
    }

    #[test]
    fn placements_expire() {
        let now = Instant::now();
        let mut placements = SpawnPlacements::default();
        let token = make_token("a");
        placements.insert(token.clone(), placement(1), now);

        let almost = now + SPAWN_PLACEMENT_TIMEOUT - Duration::from_millis(1);
        assert!(placements.contains(&token, almost));

        let later = now + SPAWN_PLACEMENT_TIMEOUT;
        assert!(!placements.contains(&token, later));
        assert_eq!(placements.take(&token, later), None);
        // Taking an expired placement still removes it.
        assert!(placements.is_empty());

        placements.insert(token.clone(), placement(1), now);
        placements.remove_expired(later);
        assert!(placements.is_empty());
    }

    #[test]
    fn placements_are_bounded() {
        let now = Instant::now();
        let mut placements = SpawnPlacements::default();
        let tokens: Vec<_> = (0..MAX_PENDING_SPAWN_PLACEMENTS + 1)
            .map(|idx| make_token(&format!("token-{idx}")))
            .collect();
        for (idx, token) in tokens.iter().enumerate() {
            placements.insert(token.clone(), placement(idx + 1), now);
        }

        assert_eq!(placements.len(), MAX_PENDING_SPAWN_PLACEMENTS);
        // The oldest one was dropped.
        assert!(!placements.contains(&tokens[0], now));
        assert!(placements.contains(&tokens[1], now));
        assert!(placements.contains(tokens.last().unwrap(), now));
    }
}

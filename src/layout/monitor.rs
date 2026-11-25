use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::iter::zip;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use niri_config::{Color, CornerRadius, LayoutPart};
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use super::insert_hint_element::{InsertHintElement, InsertHintRenderElement};
use super::overview_caption::{
    caption_background, caption_ink, frame_radius, tint_caption, FocusMarker, GroupStyle,
    LabelKind, PreviewLabel, OVERVIEW_CONTROL_HEIGHT, SCROLL_BUTTON_HEIGHT, SCROLL_CARD_HEIGHT,
    STRIP_BUTTON_HEIGHT, STRIP_FORECAST_HEIGHT, STRIP_GROUP_BUTTON_HEIGHT, STRIP_STATUS_HEIGHT,
    STRIP_TRAY_HEIGHT, STRIP_TRAY_PADDING,
};
use super::scrolling::{navigation_strip_fits, Column, ColumnWidth, NAVIGATION_STRIP_HEIGHT};
use super::tile::Tile;
use super::workspace::{
    compute_working_area, compute_workspace_shadow_config, OutputId, Workspace,
    WorkspaceAddWindowTarget, WorkspaceId, WorkspaceRenderElement,
};
use super::{
    compute_overview_zoom, ActivateWindow, HitType, LayoutElement, LayoutElementRenderElement,
    Options,
};
use crate::animation::{Animation, Clock};
use crate::input::swipe_tracker::SwipeTracker;
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::rubber_band::RubberBand;
use crate::utils::transaction::Transaction;
use crate::utils::{
    output_size, round_logical_in_physical, round_logical_in_physical_max1, ResizeEdge,
};

/// Amount of touchpad movement to scroll the height of one workspace.
const WORKSPACE_GESTURE_MOVEMENT: f64 = 300.;

const OVERVIEW_INSET: f64 = 20.;
const OVERVIEW_HEADER_OFFSET: f64 = 67.;
const OVERVIEW_FRAME_MARGIN: f64 = 12.;
const MAX_NAVIGATION_TITLES: usize = 5;

const WORKSPACE_GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

/// Amount of DnD edge scrolling to scroll the height of one workspace.
///
/// This constant is tied to the default dnd-edge-workspace-switch max-speed setting.
const WORKSPACE_DND_EDGE_SCROLL_MOVEMENT: f64 = 1500.;

#[derive(Debug)]
pub struct Monitor<W: LayoutElement> {
    /// Output for this monitor.
    pub(super) output: Output,
    /// Cached name of the output.
    output_name: String,
    /// Latest known scale for this output.
    scale: smithay::output::Scale,
    /// Latest known size for this output.
    view_size: Size<f64, Logical>,
    /// Latest known working area for this output.
    ///
    /// Not rounded to physical pixels.
    // FIXME: since this is used for things like DnD scrolling edges in the overview, ideally this
    // should only consider overlay and top layer-shell surfaces. However, Smithay doesn't easily
    // let you do this at the moment.
    working_area: Rectangle<f64, Logical>,
    // Must always contain at least one.
    pub(super) workspaces: Vec<Workspace<W>>,
    /// Index of the currently active workspace.
    pub(super) active_workspace_idx: usize,
    /// ID of the previously active workspace.
    pub(super) previous_workspace_id: Option<WorkspaceId>,
    /// In-progress switch between workspaces.
    pub(super) workspace_switch: Option<WorkspaceSwitch>,
    /// Indication where an interactively-moved window is about to be placed.
    pub(super) insert_hint: Option<InsertHint>,
    /// Insert hint element for rendering.
    insert_hint_element: InsertHintElement,
    strip_tray: PreviewLabel,
    strip_labels: [PreviewLabel; MAX_NAVIGATION_TITLES],
    strip_viewport_marks: [RefCell<SolidColorBuffer>; 3 * MAX_NAVIGATION_TITLES],
    strip_status_label: PreviewLabel,
    strip_forecast_label: PreviewLabel,
    strip_forecast_titles: [PreviewLabel; MAX_NAVIGATION_TITLES],
    strip_edge_labels: [PreviewLabel; 2],
    layer_action_label: PreviewLabel,
    overview_command_label: PreviewLabel,
    overview_shortcuts_label: PreviewLabel,
    scroll_cue_labels: [PreviewLabel; 2],
    additional_scroll_cue_labels: [PreviewLabel; 2],
    scroll_edge_fades: [PreviewLabel; 2],
    /// Location to render the insert hint element.
    insert_hint_render_loc: Option<InsertHintRenderLoc>,
    /// Whether the overview is open.
    pub(super) overview_open: bool,
    /// Progress of the overview zoom animation, 1 is fully in overview.
    overview_progress: Option<OverviewProgress>,
    overview_pointer_selection: Option<W::Id>,
    /// Clock for driving animations.
    pub(super) clock: Clock,
    /// Configurable properties of the layout as received from the parent layout.
    pub(super) base_options: Rc<Options>,
    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,
    /// Layout config overrides for this monitor.
    layout_config: Option<niri_config::LayoutPart>,
}

#[derive(Debug)]
pub enum WorkspaceSwitch {
    Animation(Animation),
    Gesture(WorkspaceSwitchGesture),
}

#[derive(Debug)]
pub struct WorkspaceSwitchGesture {
    /// Index of the workspace where the gesture was started.
    center_idx: usize,
    /// Fractional workspace index where the gesture was started.
    ///
    /// Can differ from center_idx when starting a gesture in the middle between workspaces, for
    /// example by "catching" an animation.
    start_idx: f64,
    /// Current, fractional workspace index.
    pub(super) current_idx: f64,
    /// Animation for the extra offset to the current position.
    ///
    /// For example, if there's a workspace switch during a DnD scroll.
    animation: Option<Animation>,
    tracker: SwipeTracker,
    /// Whether the gesture is controlled by the touchpad.
    is_touchpad: bool,
    /// Whether the gesture is clamped to +-1 workspace around the center.
    is_clamped: bool,

    // If this gesture is for drag-and-drop scrolling, this is the last event's unadjusted
    // timestamp.
    dnd_last_event_time: Option<Duration>,
    // Time when the drag-and-drop scroll delta became non-zero, used for debouncing.
    //
    // If `None` then the scroll delta is currently zero.
    dnd_nonzero_start_time: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertPosition {
    NewColumn(usize),
    InColumn(usize, usize),
    Floating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertWorkspace {
    Existing(WorkspaceId),
    NewAt(usize),
}

#[derive(Debug)]
pub(super) struct InsertHint {
    pub workspace: InsertWorkspace,
    pub position: InsertPosition,
    pub corner_radius: CornerRadius,
}

#[derive(Debug, Clone, Copy)]
struct InsertHintRenderLoc {
    workspace: InsertWorkspace,
    location: Point<f64, Logical>,
}

struct OverviewGroup {
    range: Range<f64>,
    title: &'static str,
    minimum_width: f64,
    style: GroupStyle,
}

struct NavigationStrip<'a, W> {
    tray: Option<Rectangle<f64, Logical>>,
    viewports: Vec<Rectangle<f64, Logical>>,
    moving: bool,
    edge_actions: [Option<NavigationArrow<'a, W>>; 2],
    titles: Vec<NavigationTitle<'a, W>>,
    layer_action: Option<(Rectangle<f64, Logical>, &'a W)>,
}

struct NavigationArrow<'a, W> {
    rect: Rectangle<f64, Logical>,
    target: Option<&'a W>,
}

struct NavigationTitle<'a, W> {
    rect: Rectangle<f64, Logical>,
    window: &'a W,
    companions: Vec<&'a W>,
    visible_after: Vec<&'a W>,
    active: bool,
    direction: Option<bool>,
    fully_visible: bool,
    floating: bool,
}

struct ScrollCue<'a, W> {
    rect: Rectangle<f64, Logical>,
    destination: &'a W,
    clipped_column_edge: Option<f64>,
    opacity: f32,
    offscreen_columns: usize,
    additional: Option<(Rectangle<f64, Logical>, &'a W)>,
}

#[derive(Debug)]
pub(super) enum OverviewProgress {
    Animation(Animation),
    Value(f64),
}

/// Where to put a newly added window.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum MonitorAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// On this workspace.
    Workspace {
        /// Id of the target workspace.
        id: WorkspaceId,
        /// Override where the window will open as a new column.
        column_idx: Option<usize>,
    },
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

impl<'a, W: LayoutElement> Copy for MonitorAddWindowTarget<'a, W> {}

impl<'a, W: LayoutElement> Clone for MonitorAddWindowTarget<'a, W> {
    fn clone(&self) -> Self {
        *self
    }
}

niri_render_elements! {
    MonitorInnerRenderElement<R> => {
        Workspace = CropRenderElement<WorkspaceRenderElement<R>>,
        InsertHint = CropRenderElement<InsertHintRenderElement>,
        Decoration = InsertHintRenderElement,
        Shadow = ShadowRenderElement,
        SolidColor = SolidColorRenderElement,
        Caption = PrimaryGpuTextureRenderElement,
        WindowPreview = CropRenderElement<LayoutElementRenderElement<R>>,
    }
}

pub type MonitorRenderElement<R> =
    RelocateRenderElement<RescaleRenderElement<MonitorInnerRenderElement<R>>>;

impl WorkspaceSwitch {
    pub fn current_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.value(),
            WorkspaceSwitch::Gesture(gesture) => {
                gesture.current_idx + gesture.animation.as_ref().map_or(0., |anim| anim.value())
            }
        }
    }

    pub fn target_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.to(),
            WorkspaceSwitch::Gesture(gesture) => gesture.current_idx,
        }
    }

    pub fn offset(&mut self, delta: isize) {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.offset(delta as f64),
            WorkspaceSwitch::Gesture(gesture) => {
                if delta >= 0 {
                    gesture.center_idx += delta as usize;
                } else {
                    gesture.center_idx -= (-delta) as usize;
                }
                gesture.start_idx += delta as f64;
                gesture.current_idx += delta as f64;
            }
        }
    }

    fn is_animation_ongoing(&self) -> bool {
        match self {
            WorkspaceSwitch::Animation(_) => true,
            WorkspaceSwitch::Gesture(gesture) => gesture.animation.is_some(),
        }
    }
}

impl WorkspaceSwitchGesture {
    fn min_max(&self, workspace_count: usize) -> (f64, f64) {
        if self.is_clamped {
            let min = self.center_idx.saturating_sub(1) as f64;
            let max = (self.center_idx + 1).min(workspace_count - 1) as f64;
            (min, max)
        } else {
            (0., (workspace_count - 1) as f64)
        }
    }

    fn animate_from(&mut self, from: f64, clock: Clock, config: niri_config::Animation) {
        let current = self.animation.as_ref().map_or(0., Animation::value);
        self.animation = Some(Animation::new(clock, from + current, 0., 0., config));
    }
}

impl InsertWorkspace {
    fn existing_id(self) -> Option<WorkspaceId> {
        match self {
            InsertWorkspace::Existing(id) => Some(id),
            InsertWorkspace::NewAt(_) => None,
        }
    }
}

impl OverviewProgress {
    pub fn value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.value(),
            OverviewProgress::Value(v) => *v,
        }
    }

    pub fn clamped_value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.clamped_value(),
            OverviewProgress::Value(v) => *v,
        }
    }
}

impl From<&super::OverviewProgress> for OverviewProgress {
    fn from(value: &super::OverviewProgress) -> Self {
        match value {
            super::OverviewProgress::Animation(anim) => Self::Animation(anim.clone()),
            super::OverviewProgress::Gesture(gesture) => Self::Value(gesture.value),
            super::OverviewProgress::Open => Self::Value(1.),
        }
    }
}

fn navigation_title<W: LayoutElement>(window: &W, target: RenderTarget, fallback: &str) -> String {
    if target.should_block_out(window.rules().block_out_from) {
        return fallback.into();
    }
    window.with_overview_title(|title| {
        let title: String = title
            .chars()
            .take(96)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if title.trim().is_empty() {
            fallback.into()
        } else {
            title
        }
    })
}

fn window_title_suffix(title: &str) -> &str {
    title
        .rsplit_once(" — ")
        .map(|(_, suffix)| suffix.trim())
        .filter(|suffix| !suffix.is_empty())
        .unwrap_or(title)
}

fn compact_window_title(title: &str, distinct_app: bool) -> &str {
    let name = if distinct_app {
        window_title_suffix(title)
    } else {
        title
            .rsplit_once(" — ")
            .map(|(context, _)| context.trim())
            .filter(|context| !context.is_empty())
            .unwrap_or(title)
    };
    name.strip_prefix("Canopa ").unwrap_or(name)
}

fn visible_extent<'a, W: LayoutElement + 'a>(
    tiles: impl Iterator<Item = (&'a Tile<W>, Point<f64, Logical>, bool)>,
) -> Option<Range<f64>> {
    tiles
        .filter(|(_, _, shown)| *shown)
        .map(|(tile, pos, _)| pos.x..pos.x + tile.tile_size().w)
        .reduce(|extent, next| extent.start.min(next.start)..extent.end.max(next.end))
}

/// Center the complete strip in the overview when it fits at the configured
/// zoom. Wider strips keep the scrolling layout's camera position.
fn fit_overview_strip(
    extent: Range<f64>,
    viewport: &Range<f64>,
    zoom: f64,
    final_zoom: f64,
) -> Option<f64> {
    let width = extent.end - extent.start;
    let available = viewport.end - viewport.start;
    if width * final_zoom > available {
        return None;
    }
    let center = (viewport.start + viewport.end) / 2.;
    Some(center - (extent.start + extent.end) / 2. * zoom)
}

#[cfg(test)]
mod tests {
    use super::fit_overview_strip;

    #[test]
    fn overview_strip_centers_within_the_area_beside_the_rail() {
        let viewport = 100. ..1300.;
        let extent = -512. ..1408.;
        let zoom = 0.5;
        let x = fit_overview_strip(extent.clone(), &viewport, zoom, zoom).unwrap();

        let left_gap = x + extent.start * zoom - viewport.start;
        let right_gap = viewport.end - (x + extent.end * zoom);
        assert_eq!(left_gap, 120.);
        assert_eq!(left_gap, right_gap);
    }

    #[test]
    fn overview_strip_retains_the_scroll_position_if_it_cannot_fit() {
        let viewport = 100. ..1300.;
        assert_eq!(fit_overview_strip(0. ..2500., &viewport, 0.5, 0.5), None);
    }
}

impl<W: LayoutElement> Monitor<W> {
    pub fn new(
        output: Output,
        mut workspaces: Vec<Workspace<W>>,
        ws_id_to_activate: Option<WorkspaceId>,
        clock: Clock,
        base_options: Rc<Options>,
        layout_config: Option<LayoutPart>,
    ) -> Self {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(layout_config.as_ref()));

        let scale = output.current_scale();
        let view_size = output_size(&output);
        let working_area = compute_working_area(&output);

        // Prepare the workspaces: set output, empty first, empty last.
        let mut active_workspace_idx = 0;

        for (idx, ws) in workspaces.iter_mut().enumerate() {
            assert!(ws.has_windows_or_name());

            ws.set_output(Some(output.clone()));
            ws.update_config(options.clone());

            if ws_id_to_activate.is_some_and(|id| ws.id() == id) {
                active_workspace_idx = idx;
            }
        }

        if options.layout.empty_workspace_above_first && !workspaces.is_empty() {
            let ws = Workspace::new(output.clone(), clock.clone(), options.clone());
            workspaces.insert(0, ws);
            active_workspace_idx += 1;
        }

        let ws = Workspace::new(output.clone(), clock.clone(), options.clone());
        workspaces.push(ws);

        Self {
            output_name: output.name(),
            output,
            scale,
            view_size,
            working_area,
            workspaces,
            active_workspace_idx,
            previous_workspace_id: None,
            insert_hint: None,
            insert_hint_element: InsertHintElement::new(options.layout.insert_hint),
            strip_tray: PreviewLabel::default(),
            strip_labels: Default::default(),
            strip_viewport_marks: Default::default(),
            strip_status_label: PreviewLabel::default(),
            strip_forecast_label: PreviewLabel::default(),
            strip_forecast_titles: Default::default(),
            strip_edge_labels: Default::default(),
            layer_action_label: PreviewLabel::default(),
            overview_command_label: PreviewLabel::default(),
            overview_shortcuts_label: PreviewLabel::default(),
            scroll_cue_labels: Default::default(),
            additional_scroll_cue_labels: Default::default(),
            scroll_edge_fades: Default::default(),
            insert_hint_render_loc: None,
            overview_open: false,
            overview_progress: None,
            overview_pointer_selection: None,
            workspace_switch: None,
            clock,
            base_options,
            options,
            layout_config,
        }
    }

    pub fn into_workspaces(mut self) -> Vec<Workspace<W>> {
        self.workspaces.retain(|ws| ws.has_windows_or_name());

        for ws in &mut self.workspaces {
            ws.set_output(None);
        }

        self.workspaces
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn output_name(&self) -> &String {
        &self.output_name
    }

    pub fn active_workspace_idx(&self) -> usize {
        self.active_workspace_idx
    }

    pub fn active_workspace_ref(&self) -> &Workspace<W> {
        &self.workspaces[self.active_workspace_idx]
    }

    pub fn find_named_workspace(&self, workspace_name: &str) -> Option<&Workspace<W>> {
        self.workspaces.iter().find(|ws| {
            ws.name
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    pub fn find_named_workspace_index(&self, workspace_name: &str) -> Option<usize> {
        self.workspaces.iter().position(|ws| {
            ws.name
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    pub fn active_workspace(&mut self) -> &mut Workspace<W> {
        &mut self.workspaces[self.active_workspace_idx]
    }

    pub fn idx_of_ws(&self, id: WorkspaceId) -> Option<usize> {
        self.workspaces.iter().position(|ws| ws.id() == id)
    }

    pub fn has_ws(&self, id: WorkspaceId) -> bool {
        self.idx_of_ws(id).is_some()
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> {
        self.workspaces.iter().flat_map(|ws| ws.windows())
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|win| win.id() == window)
    }

    pub fn add_workspace_at(&mut self, idx: usize) {
        let ws = Workspace::new(
            self.output.clone(),
            self.clock.clone(),
            self.options.clone(),
        );

        self.workspaces.insert(idx, ws);
        if idx <= self.active_workspace_idx {
            self.active_workspace_idx += 1;
        }

        if let Some(switch) = &mut self.workspace_switch {
            if idx as f64 <= switch.target_idx() {
                switch.offset(1);
            }
        }
    }

    pub fn add_workspace_top(&mut self) {
        self.add_workspace_at(0);
    }

    pub fn add_workspace_bottom(&mut self) {
        self.add_workspace_at(self.workspaces.len());
    }

    pub fn activate_workspace(&mut self, idx: usize) {
        self.activate_workspace_with_anim_config(idx, None);
    }

    pub fn activate_workspace_with_anim_config(
        &mut self,
        idx: usize,
        config: Option<niri_config::Animation>,
    ) {
        // FIXME: also compute and use current velocity.
        let current_idx = self.workspace_render_idx();

        if self.active_workspace_idx != idx {
            self.previous_workspace_id = Some(self.workspaces[self.active_workspace_idx].id());
        }

        let prev_active_idx = self.active_workspace_idx;
        self.active_workspace_idx = idx;

        let config = config.unwrap_or(self.options.animations.workspace_switch.0);

        match &mut self.workspace_switch {
            // During a DnD scroll, we want to visually animate even if idx matches the active idx.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                gesture.center_idx = idx;

                // Adjust start_idx to make current_idx point at idx.
                let current_pos = gesture.current_idx - gesture.start_idx;
                gesture.start_idx = idx as f64 - current_pos;
                let prev_current_idx = gesture.current_idx;
                gesture.current_idx = idx as f64;

                let current_idx_delta = gesture.current_idx - prev_current_idx;
                gesture.animate_from(-current_idx_delta, self.clock.clone(), config);
            }
            _ => {
                // Don't animate if nothing changed.
                if prev_active_idx == idx {
                    return;
                }

                self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
                    self.clock.clone(),
                    current_idx,
                    idx as f64,
                    0.,
                    config,
                )));
            }
        }
    }

    pub(super) fn resolve_add_window_target<'a>(
        &mut self,
        target: MonitorAddWindowTarget<'a, W>,
    ) -> (usize, WorkspaceAddWindowTarget<'a, W>) {
        match target {
            MonitorAddWindowTarget::Auto => {
                (self.active_workspace_idx, WorkspaceAddWindowTarget::Auto)
            }
            MonitorAddWindowTarget::Workspace { id, column_idx } => {
                let idx = self.idx_of_ws(id).unwrap();
                let target = if let Some(column_idx) = column_idx {
                    WorkspaceAddWindowTarget::NewColumnAt(column_idx)
                } else {
                    WorkspaceAddWindowTarget::Auto
                };
                (idx, target)
            }
            MonitorAddWindowTarget::NextTo(win_id) => {
                let idx = self
                    .workspaces
                    .iter_mut()
                    .position(|ws| ws.has_window(win_id))
                    .unwrap();
                (idx, WorkspaceAddWindowTarget::NextTo(win_id))
            }
        }
    }

    pub fn add_window(
        &mut self,
        window: W,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        // Currently, everything a workspace sets on a Tile is the same across all workspaces of a
        // monitor. So we can use any workspace, not necessarily the exact target workspace.
        let tile = self.workspaces[0].make_tile(window);

        self.add_tile(
            tile,
            target,
            activate,
            true,
            width,
            is_full_width,
            is_floating,
            None,
        );
    }

    pub fn add_column(
        &mut self,
        mut workspace_idx: usize,
        column: Column<W>,
        activate: bool,
        anim: Option<niri_config::Animation>,
    ) {
        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_column(column, activate, anim);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_outputs = vec![OutputId::new(&self.output)];
        }

        if workspace_idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
        }
        if self.options.layout.empty_workspace_above_first && workspace_idx == 0 {
            self.add_workspace_top();
            workspace_idx += 1;
        }

        if activate {
            self.activate_workspace(workspace_idx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_tile(
        &mut self,
        tile: Tile<W>,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        // FIXME: Refactor ActivateWindow enum to make this better.
        allow_to_activate_workspace: bool,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
        anim: Option<niri_config::Animation>,
    ) {
        let (mut workspace_idx, target) = self.resolve_add_window_target(target);

        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_tile(
            tile,
            target,
            activate,
            width,
            is_full_width,
            is_floating,
            anim,
        );

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_outputs = vec![OutputId::new(&self.output)];
        }

        if workspace_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && workspace_idx == 0 {
            self.add_workspace_top();
            workspace_idx += 1;
        }

        if allow_to_activate_workspace && activate.map_smart(|| false) {
            self.activate_workspace(workspace_idx);
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        workspace_idx: usize,
        column_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        // FIXME: Refactor ActivateWindow enum to make this better.
        allow_to_activate_workspace: bool,
    ) {
        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_tile_to_column(column_idx, tile_idx, tile, activate);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_outputs = vec![OutputId::new(&self.output)];
        }

        // Since we're adding window to an existing column, the workspace isn't empty, and
        // therefore cannot be the last one, so we never need to insert a new empty workspace.

        if allow_to_activate_workspace && activate {
            self.activate_workspace(workspace_idx);
        }
    }

    pub fn clean_up_workspaces(&mut self) {
        assert!(self.workspace_switch.is_none());

        let range_start = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        for idx in (range_start..self.workspaces.len() - 1).rev() {
            if self.active_workspace_idx == idx {
                continue;
            }

            if !self.workspaces[idx].has_windows_or_name() {
                self.workspaces.remove(idx);
                if self.active_workspace_idx > idx {
                    self.active_workspace_idx -= 1;
                }
            }
        }

        // Special case handling when empty_workspace_above_first is set and all workspaces
        // are empty.
        if self.options.layout.empty_workspace_above_first && self.workspaces.len() == 2 {
            assert!(!self.workspaces[0].has_windows_or_name());
            assert!(!self.workspaces[1].has_windows_or_name());
            self.workspaces.remove(1);
            self.active_workspace_idx = 0;
        }
    }

    pub fn unname_workspace(&mut self, id: WorkspaceId) -> bool {
        let Some(idx) = self.idx_of_ws(id) else {
            return false;
        };
        let ws = &mut self.workspaces[idx];

        ws.unname();

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces();
        }

        true
    }

    pub fn remove_workspace_by_idx(&mut self, mut idx: usize) -> Workspace<W> {
        if idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
        }
        if self.options.layout.empty_workspace_above_first && idx == 0 {
            self.add_workspace_top();
            idx += 1;
        }

        let mut ws = self.workspaces.remove(idx);
        ws.set_output(None);

        // For monitor current workspace removal, we focus previous rather than next (<= rather
        // than <). This is different from columns and tiles, but it lets move-workspace-to-monitor
        // back and forth to preserve position.
        if idx <= self.active_workspace_idx && self.active_workspace_idx > 0 {
            self.active_workspace_idx -= 1;
        }

        self.workspace_switch = None;
        self.clean_up_workspaces();

        ws
    }

    pub fn insert_workspace(&mut self, mut ws: Workspace<W>, mut idx: usize, activate: bool) {
        ws.set_output(Some(self.output.clone()));
        ws.update_config(self.options.clone());

        // Don't insert past the last empty workspace.
        if idx == self.workspaces.len() {
            idx -= 1;
        }
        if idx == 0 && self.options.layout.empty_workspace_above_first {
            // Insert a new empty workspace on top to prepare for insertion of new workspace.
            self.add_workspace_top();
            idx += 1;
        }

        self.workspaces.insert(idx, ws);

        if idx <= self.active_workspace_idx {
            self.active_workspace_idx += 1;
        }

        if activate {
            self.workspace_switch = None;
            self.activate_workspace(idx);
        }

        self.workspace_switch = None;
        self.clean_up_workspaces();
    }

    pub fn append_workspaces(&mut self, mut workspaces: Vec<Workspace<W>>) {
        if workspaces.is_empty() {
            return;
        }

        for ws in &mut workspaces {
            ws.set_output(Some(self.output.clone()));
            ws.update_config(self.options.clone());
        }

        let empty_was_focused = self.active_workspace_idx == self.workspaces.len() - 1;

        // Push the workspaces from the removed monitor in the end, right before the
        // last, empty, workspace.
        let empty = self.workspaces.remove(self.workspaces.len() - 1);
        self.workspaces.extend(workspaces);
        self.workspaces.push(empty);

        // If empty_workspace_above_first is set and the first workspace is now no longer empty,
        // add a new empty workspace on top.
        if self.options.layout.empty_workspace_above_first
            && self.workspaces[0].has_windows_or_name()
        {
            self.add_workspace_top();
        }

        // If the empty workspace was focused on the primary monitor, keep it focused.
        if empty_was_focused {
            self.active_workspace_idx = self.workspaces.len() - 1;
        }

        // FIXME: if we're adding workspaces to currently invisible positions
        // (outside the workspace switch), we don't need to cancel it.
        self.workspace_switch = None;
        self.clean_up_workspaces();
    }

    pub fn move_down_or_to_workspace_down(&mut self) {
        if !self.active_workspace().move_down() {
            self.move_to_workspace_down(ActivateWindow::Smart);
        }
    }

    pub fn move_up_or_to_workspace_up(&mut self) {
        if !self.active_workspace().move_up() {
            self.move_to_workspace_up(ActivateWindow::Smart);
        }
    }

    pub fn focus_window_or_workspace_down(&mut self) {
        if !self.active_workspace().focus_down() {
            self.switch_workspace_down();
        }
    }

    pub fn focus_window_or_workspace_up(&mut self) {
        if !self.active_workspace().focus_up() {
            self.switch_workspace_up();
        }
    }

    pub fn move_to_workspace_up(&mut self, activate: ActivateWindow) {
        let new_idx = self.active_workspace_idx.saturating_sub(1);
        self.move_to_workspace(None, new_idx, activate);
    }

    pub fn move_to_workspace_down(&mut self, activate: ActivateWindow) {
        let new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        self.move_to_workspace(None, new_idx, activate);
    }

    pub fn move_to_workspace(
        &mut self,
        window: Option<&W::Id>,
        idx: usize,
        activate: ActivateWindow,
    ) {
        let source_workspace_idx = if let Some(window) = window {
            self.workspaces
                .iter()
                .position(|ws| ws.has_window(window))
                .unwrap()
        } else {
            self.active_workspace_idx
        };
        let source_id = self.workspaces[source_workspace_idx].id();

        let new_idx = min(idx, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }
        let new_id = self.workspaces[new_idx].id();

        let activate = activate.map_smart(|| {
            window.is_none_or(|win| self.active_window().map(|win| win.id()) == Some(win))
        });

        let workspace = &mut self.workspaces[source_workspace_idx];
        let Some(window) = window.or_else(|| workspace.active_window().map(|win| win.id())) else {
            return;
        };
        let window = window.clone();

        let mut old_render_pos = workspace
            .tiles_with_render_positions()
            .find_map(|(tile, offset, _visible)| (tile.window().id() == &window).then_some(offset))
            .unwrap();

        let transaction = Transaction::new();
        let removed = workspace.remove_tile(&window, transaction);

        // If the view is following the tile, match the animation.
        let config = if activate {
            self.options.animations.workspace_switch.0
        } else {
            self.options.animations.window_movement.0
        };

        self.add_tile(
            removed.tile,
            MonitorAddWindowTarget::Workspace {
                id: new_id,
                column_idx: None,
            },
            if activate {
                ActivateWindow::Yes
            } else {
                ActivateWindow::No
            },
            true,
            removed.width,
            removed.is_full_width,
            removed.is_floating,
            Some(config),
        );

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces();
        }

        let new_idx = self.idx_of_ws(new_id).unwrap();

        // Animate vertical movement between workspaces.
        //
        // Recompute the source idx in case some workspace was removed during clean-up. If the
        // source workspace itself was removed, don't bother animating this since the removal is
        // instant anyway.
        if let Some(source_workspace_idx) = self.idx_of_ws(source_id) {
            old_render_pos.y +=
                self.workspace_size_with_gap(1.).h * (source_workspace_idx as f64 - new_idx as f64);
        }

        let (tile, new_render_pos) = self.workspaces[new_idx]
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == &window)
            .unwrap();
        tile.animate_move_from_with_config(old_render_pos - new_render_pos, config);
        tile.set_anim_y_between_workspaces();
    }

    pub fn move_column_to_workspace_up(&mut self, activate: bool) {
        let new_idx = self.active_workspace_idx.saturating_sub(1);
        self.move_column_to_workspace(new_idx, activate);
    }

    pub fn move_column_to_workspace_down(&mut self, activate: bool) {
        let new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        self.move_column_to_workspace(new_idx, activate);
    }

    pub fn move_column_to_workspace(&mut self, idx: usize, activate: bool) {
        let source_workspace_idx = self.active_workspace_idx;

        let new_idx = min(idx, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = &mut self.workspaces[source_workspace_idx];
        if workspace.floating_is_active() {
            let activate = if activate {
                ActivateWindow::Smart
            } else {
                ActivateWindow::No
            };
            self.move_to_workspace(None, idx, activate);
            return;
        }

        let Some(id) = workspace.scrolling().active_column().map(Column::id) else {
            return;
        };
        let mut old_render_pos = workspace
            .scrolling()
            .columns_with_render_positions()
            .find_map(|(col, pos)| (col.id() == id).then_some(pos))
            .unwrap();

        let column = workspace.remove_active_column().unwrap();

        // Animate vertical movement between workspaces.
        old_render_pos.y +=
            self.workspace_size_with_gap(1.).h * (source_workspace_idx as f64 - new_idx as f64);

        // If the view is following the column, match the animation.
        let config = if activate {
            self.options.animations.workspace_switch.0
        } else {
            self.options.animations.window_movement.0
        };

        let new_id = self.workspaces[new_idx].id();
        self.add_column(new_idx, column, activate, Some(config));

        let new_idx = self.idx_of_ws(new_id).unwrap();
        let (column, new_render_pos) = self.workspaces[new_idx]
            .scrolling_mut()
            .columns_with_render_positions_mut()
            .find(|(col, _pos)| col.id() == id)
            .unwrap();
        column.animate_move_from_with_config(old_render_pos - new_render_pos, config);
        column.set_anim_y_between_workspaces();
    }

    pub fn switch_workspace_up(&mut self) {
        let new_idx = match &self.workspace_switch {
            // During a DnD scroll, select the prev apparent workspace.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                let current = gesture.current_idx;
                let new = current.ceil() - 1.;
                new.clamp(0., (self.workspaces.len() - 1) as f64) as usize
            }
            _ => self.active_workspace_idx.saturating_sub(1),
        };

        self.activate_workspace(new_idx);
    }

    pub fn switch_workspace_down(&mut self) {
        let new_idx = match &self.workspace_switch {
            // During a DnD scroll, select the next apparent workspace.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                let current = gesture.current_idx;
                let new = current.floor() + 1.;
                new.clamp(0., (self.workspaces.len() - 1) as f64) as usize
            }
            _ => min(self.active_workspace_idx + 1, self.workspaces.len() - 1),
        };

        self.activate_workspace(new_idx);
    }

    fn previous_workspace_idx(&self) -> Option<usize> {
        let id = self.previous_workspace_id?;
        self.idx_of_ws(id)
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        self.activate_workspace(min(idx, self.workspaces.len() - 1));
    }

    pub fn switch_workspace_auto_back_and_forth(&mut self, idx: usize) {
        let idx = min(idx, self.workspaces.len() - 1);

        if idx == self.active_workspace_idx {
            if let Some(prev_idx) = self.previous_workspace_idx() {
                self.switch_workspace(prev_idx);
            }
        } else {
            self.switch_workspace(idx);
        }
    }

    pub fn switch_workspace_previous(&mut self) {
        if let Some(idx) = self.previous_workspace_idx() {
            self.switch_workspace(idx);
        }
    }

    pub fn active_window(&self) -> Option<&W> {
        self.active_workspace_ref().active_window()
    }

    pub fn advance_animations(&mut self) {
        match &mut self.workspace_switch {
            Some(WorkspaceSwitch::Animation(anim)) => {
                if anim.is_done() {
                    self.workspace_switch = None;
                    self.clean_up_workspaces();
                }
            }
            Some(WorkspaceSwitch::Gesture(gesture)) => {
                // Make sure the last event time doesn't go too much out of date (for
                // monitors not under cursor), causing sudden jumps.
                //
                // This happens after any dnd_scroll_gesture_scroll() calls (in
                // Layout::advance_animations()), so it doesn't mess up the time delta there.
                if let Some(last_time) = &mut gesture.dnd_last_event_time {
                    let now = self.clock.now_unadjusted();
                    if *last_time != now {
                        *last_time = now;

                        // If last_time was already == now, then dnd_scroll_gesture_scroll() must've
                        // updated the gesture already. Therefore, when this code runs, the pointer
                        // must be outside the DnD scrolling zone.
                        gesture.dnd_nonzero_start_time = None;
                    }
                }

                if let Some(anim) = &mut gesture.animation {
                    if anim.is_done() {
                        gesture.animation = None;
                    }
                }
            }
            None => (),
        }

        for ws in &mut self.workspaces {
            ws.advance_animations();
        }
    }

    pub(super) fn are_animations_ongoing(&self) -> bool {
        self.workspace_switch
            .as_ref()
            .is_some_and(|s| s.is_animation_ongoing())
            || self.workspaces.iter().any(|ws| ws.are_animations_ongoing())
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.workspace_switch.is_some()
            || self
                .workspaces
                .iter()
                .any(|ws| ws.are_transitions_ongoing())
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        let showing_previews = self.has_framed_overview()
            && self
                .overview_progress_value()
                .is_some_and(|progress| progress >= 0.82);
        let is_active = is_active && !showing_previews;
        let mut insert_hint_ws_geo = None;
        let insert_hint_ws_id = self
            .insert_hint
            .as_ref()
            .and_then(|hint| hint.workspace.existing_id());

        for ws in &mut self.workspaces {
            ws.update_render_elements(is_active, RenderLayer::MovingBetweenWorkspaces);
        }

        for (ws, geo) in self.workspaces_with_render_geo_mut(true) {
            ws.update_render_elements(is_active, RenderLayer::Normal);

            if Some(ws.id()) == insert_hint_ws_id {
                insert_hint_ws_geo = Some(geo);
            }
        }

        self.insert_hint_render_loc = None;
        if let Some(hint) = &self.insert_hint {
            match hint.workspace {
                InsertWorkspace::Existing(ws_id) => {
                    if let Some(idx) = self.idx_of_ws(ws_id) {
                        let ws = &self.workspaces[idx];
                        if let Some(mut area) = ws.insert_hint_area(hint.position) {
                            let scale = ws.scale().fractional_scale();
                            let view_size = ws.view_size();

                            // Make sure the hint is at least partially visible.
                            if matches!(hint.position, InsertPosition::NewColumn(_)) {
                                let zoom = self.overview_zoom();
                                let geo = insert_hint_ws_geo.unwrap();
                                let geo = geo.downscale(zoom);

                                area.loc.x = area.loc.x.max(-geo.loc.x - area.size.w / 2.);
                                area.loc.x =
                                    area.loc.x.min(geo.loc.x + geo.size.w - area.size.w / 2.);
                            }

                            // Round to physical pixels.
                            area = area.to_physical_precise_round(scale).to_logical(scale);

                            let view_rect = Rectangle::new(area.loc.upscale(-1.), view_size);
                            self.insert_hint_element.update_render_elements(
                                area.size,
                                view_rect,
                                hint.corner_radius,
                                scale,
                            );
                            self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                                workspace: hint.workspace,
                                location: area.loc,
                            });
                        }
                    } else {
                        error!("insert hint workspace missing from monitor");
                    }
                }
                InsertWorkspace::NewAt(ws_idx) => {
                    let scale = self.scale.fractional_scale();
                    let zoom = self.overview_zoom();
                    let gap = self.workspace_gap(zoom);

                    let hint_gap = round_logical_in_physical(scale, gap * 0.1);
                    let hint_height = gap - hint_gap * 2.;

                    let next_ws_geo = self.workspaces_render_geo().nth(ws_idx).unwrap();
                    let hint_width = round_logical_in_physical(scale, next_ws_geo.size.w * 0.75);
                    let hint_x =
                        round_logical_in_physical(scale, (next_ws_geo.size.w - hint_width) / 2.);

                    let hint_loc_diff = Point::from((-hint_x, hint_height + hint_gap));
                    let hint_loc = next_ws_geo.loc - hint_loc_diff;
                    let hint_size = Size::from((hint_width, hint_height));

                    // Sometimes the hint ends up 1 px wider than necessary and/or 1 px
                    // narrower than necessary. The values here seem correct. Might have to do with
                    // how zooming out currently doesn't round to output scale properly.

                    // Compute view rect as if we're above the next workspace (rather than below
                    // the previous one).
                    let view_rect = Rectangle::new(hint_loc_diff, next_ws_geo.size);

                    self.insert_hint_element.update_render_elements(
                        hint_size,
                        view_rect,
                        CornerRadius::default(),
                        scale,
                    );
                    self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                        workspace: hint.workspace,
                        location: hint_loc,
                    });
                }
            }
        }
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(self.layout_config.as_ref()));

        if self.options.layout.empty_workspace_above_first
            != options.layout.empty_workspace_above_first
            && self.workspaces.len() > 1
        {
            if options.layout.empty_workspace_above_first {
                self.add_workspace_top();
            } else if self.workspace_switch.is_none() && self.active_workspace_idx != 0 {
                self.workspaces.remove(0);
                self.active_workspace_idx = self.active_workspace_idx.saturating_sub(1);
            }
        }

        for ws in &mut self.workspaces {
            ws.update_config(options.clone());
        }

        self.insert_hint_element
            .update_config(options.layout.insert_hint);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(&mut self, layout_config: Option<niri_config::LayoutPart>) -> bool {
        if self.layout_config == layout_config {
            return false;
        }

        self.layout_config = layout_config;
        self.update_config(self.base_options.clone());

        true
    }

    pub fn update_shaders(&mut self) {
        for ws in &mut self.workspaces {
            ws.update_shaders();
        }

        self.insert_hint_element.update_shaders();
    }

    pub fn update_output_size(&mut self) {
        self.scale = self.output.current_scale();
        self.view_size = output_size(&self.output);
        self.working_area = compute_working_area(&self.output);

        for ws in &mut self.workspaces {
            ws.update_output_size();
        }
    }

    pub fn move_workspace_down(&mut self) {
        let mut new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if new_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && self.active_workspace_idx == 0 {
            self.add_workspace_top();
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces();
    }

    pub fn move_workspace_up(&mut self) {
        let mut new_idx = self.active_workspace_idx.saturating_sub(1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if self.active_workspace_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && new_idx == 0 {
            self.add_workspace_top();
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces();
    }

    pub fn move_workspace_to_idx(&mut self, old_idx: usize, new_idx: usize) {
        if self.workspaces.len() <= old_idx {
            return;
        }

        let mut new_idx = new_idx.clamp(0, self.workspaces.len() - 1);
        if old_idx == new_idx {
            return;
        }

        let ws = self.workspaces.remove(old_idx);
        self.workspaces.insert(new_idx, ws);

        if new_idx > old_idx {
            if new_idx == self.workspaces.len() - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom();
            }

            if self.options.layout.empty_workspace_above_first && old_idx == 0 {
                self.add_workspace_top();
                new_idx += 1;
            }
        } else {
            if old_idx == self.workspaces.len() - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom();
            }

            if self.options.layout.empty_workspace_above_first && new_idx == 0 {
                self.add_workspace_top();
                new_idx += 1;
            }
        }

        // Only refocus the workspace if it was already focused
        if self.active_workspace_idx == old_idx {
            self.active_workspace_idx = new_idx;
        // If the workspace order was switched so that the current workspace moved down the
        // workspace stack, focus correctly
        } else if new_idx <= self.active_workspace_idx && old_idx > self.active_workspace_idx {
            self.active_workspace_idx += 1;
        } else if new_idx >= self.active_workspace_idx && old_idx < self.active_workspace_idx {
            self.active_workspace_idx = self.active_workspace_idx.saturating_sub(1);
        }

        self.workspace_switch = None;

        self.clean_up_workspaces();
    }

    /// Returns the geometry of the active window relative to and clamped to the output.
    ///
    /// During animations, assumes the final view position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.overview_open {
            return None;
        }

        self.active_workspace_ref().active_window_visual_rectangle()
    }

    fn workspace_size(&self, zoom: f64) -> Size<f64, Logical> {
        let ws_size = self.view_size.upscale(zoom);
        let scale = self.scale.fractional_scale();
        ws_size.to_physical_precise_ceil(scale).to_logical(scale)
    }

    fn workspace_gap(&self, zoom: f64) -> f64 {
        let scale = self.scale.fractional_scale();
        let gap = self.view_size.h * 0.1 * zoom;
        round_logical_in_physical_max1(scale, gap)
    }

    fn workspace_size_with_gap(&self, zoom: f64) -> Size<f64, Logical> {
        let gap = self.workspace_gap(zoom);
        self.workspace_size(zoom) + Size::from((0., gap))
    }

    pub fn overview_zoom(&self) -> f64 {
        let progress = self.overview_progress.as_ref().map(|p| p.value());
        compute_overview_zoom(&self.options, progress)
    }

    pub(super) fn set_overview_progress(&mut self, progress: Option<&super::OverviewProgress>) {
        if !self.overview_open {
            self.overview_pointer_selection = None;
        }
        let prev_render_idx = self.workspace_render_idx();
        self.overview_progress = progress.map(OverviewProgress::from);
        let new_render_idx = self.workspace_render_idx();

        // If the view jumped (can happen when going from corrected to uncorrected render_idx, for
        // example when toggling the overview in the middle of an overview animation), then restart
        // the workspace switch to avoid jumps.
        if prev_render_idx != new_render_idx {
            if let Some(WorkspaceSwitch::Animation(anim)) = &mut self.workspace_switch {
                // FIXME: maintain velocity.
                *anim = anim.restarted(prev_render_idx, anim.to(), 0.);
            }
        }
    }

    pub(super) fn overview_progress_value(&self) -> Option<f64> {
        self.overview_progress.as_ref().map(|p| p.value())
    }

    pub fn workspace_render_idx(&self) -> f64 {
        // If workspace switch and overview progress are matching animations, then compute a
        // correction term to make the movement appear monotonic.
        if let (
            Some(WorkspaceSwitch::Animation(switch_anim)),
            Some(OverviewProgress::Animation(progress_anim)),
        ) = (&self.workspace_switch, &self.overview_progress)
        {
            if switch_anim.start_time() == progress_anim.start_time()
                && (switch_anim.duration().as_secs_f64() - progress_anim.duration().as_secs_f64())
                    .abs()
                    <= 0.001
            {
                #[rustfmt::skip]
                // How this was derived:
                //
                // - Assume we're animating a zoom + switch. Consider switch "from" and "to".
                //   These are render_idx values, so first workspace to second would have switch
                //   from = 0. and to = 1. regardless of the zoom level.
                //
                // - At the start, the point at "from" is at Y = 0. We're moving the point at "to"
                //   to Y = 0. We want this to be a monotonic motion in apparent coordinates (after
                //   zoom).
                //
                // - Height at the start:
                //   from_height = (size.h + gap) * from_zoom.
                //
                // - Current height:
                //   current_height = (size.h + gap) * zoom.
                //
                // - We're moving the "to" point to Y = 0:
                //   to_y = 0.
                //
                // - The initial position of the point we're moving:
                //   from_y = (to - from) * from_height.
                //
                // - We want this point to travel monotonically in apparent coordinates:
                //   current_y = from_y + (to_y - from_y) * progress,
                //   where progress is from 0 to 1, equals to the animation progress (switch and
                //   zoom are the same since they are synchronized).
                //
                // - Derive the Y of the first workspace from this:
                //   first_y = current_y - to * current_height.
                //
                // Now, let's substitute and rearrange the terms.
                //
                // - current_y = from_y + (0 - (to - from) * from_height) * progress
                // - progress = (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - (to - from) * from_height * (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - from_height * (switch_anim.value() - from)
                // - first_y = from_y - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = (to - from) * from_height - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = to * from_height - switch_anim.value() * from_height - to * current_height
                // - first_y = -switch_anim.value() * from_height + to * (from_height - current_height)
                let from = progress_anim.from();
                let from_zoom = compute_overview_zoom(&self.options, Some(from));
                let from_ws_height_with_gap = self.workspace_size_with_gap(from_zoom).h;

                let zoom = self.overview_zoom();
                let ws_height_with_gap = self.workspace_size_with_gap(zoom).h;

                let first_ws_y = -switch_anim.value() * from_ws_height_with_gap
                    + switch_anim.to() * (from_ws_height_with_gap - ws_height_with_gap);

                return -first_ws_y / ws_height_with_gap;
            }
        };

        if let Some(switch) = &self.workspace_switch {
            switch.current_idx()
        } else {
            self.active_workspace_idx as f64
        }
    }

    fn floating_overview_positions(
        &self,
        ws: &Workspace<W>,
        progress: f64,
    ) -> Vec<(W::Id, Point<f64, Logical>)> {
        if !self.has_framed_overview() || progress <= 0. || ws.floating().is_empty() {
            return Vec::new();
        }
        let extent = visible_extent(ws.scrolling().tiles_with_render_positions());
        let gap = ws.options.layout.gaps.max(22.);
        let y = ws.working_area().loc.y + ws.options.layout.gaps + self.overview_strip_offset(ws);
        let left = extent.as_ref().map_or(0., |range| range.start);
        let mut x = extent
            .as_ref()
            .map_or(left, |range| range.end + gap.max(42.));
        let mut floating = ws
            .floating()
            .tiles_with_render_positions()
            .collect::<Vec<_>>();
        floating.sort_by(|(_, a), (_, b)| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
        let total_width = x - left
            + floating
                .iter()
                .map(|(tile, _)| tile.tile_size().w + gap)
                .sum::<f64>()
            - gap;
        let final_zoom = self.options.overview.zoom.clamp(0.0001, 0.75);
        if total_width * final_zoom > self.overview_usable_area(1.).size.w - 2. * OVERVIEW_INSET {
            return Vec::new();
        }

        let scale = self.scale.fractional_scale();
        let progress = progress.clamp(0., 1.);
        floating
            .into_iter()
            .map(|(tile, current)| {
                let destination = Point::from((x, y)) + tile.interactive_move_offset;
                x += tile.tile_size().w + gap;
                let position = current + (destination - current).upscale(progress);
                let position = position.to_physical_precise_round(scale).to_logical(scale);
                (tile.window().id().clone(), position)
            })
            .collect()
    }

    fn overview_strip_offset(&self, ws: &Workspace<W>) -> f64 {
        if ws.options.layout.clip_scroll_to_work_area
            && !ws.scrolling().is_empty()
            && self.navigation_strip_top().is_some()
        {
            NAVIGATION_STRIP_HEIGHT
        } else {
            0.
        }
    }

    pub(super) fn floating_overview_position(
        &self,
        ws: &Workspace<W>,
        window: &W::Id,
    ) -> Option<Point<f64, Logical>> {
        let progress = self.overview_progress_value()?;
        self.floating_overview_positions(ws, progress)
            .into_iter()
            .find_map(|(id, position)| (id == *window).then_some(position))
    }

    pub fn workspaces_render_geo(&self) -> impl Iterator<Item = Rectangle<f64, Logical>> {
        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();

        let ws_size = self.workspace_size(zoom);
        let gap = self.workspace_gap(zoom);
        let ws_height_with_gap = ws_size.h + gap;

        let mut static_offset = (self.view_size.to_point() - ws_size.to_point()).downscale(2.);
        if self.options.overview.framed_workspaces {
            // Zoom toward the center of the usable area when a panel occupies an edge.
            let offset = self.working_area.loc
                + (self.working_area.size.to_point() - self.view_size.to_point()).downscale(2.);
            static_offset += offset.upscale(1. - zoom);
        }
        let static_offset = static_offset
            .to_physical_precise_round(scale)
            .to_logical(scale);

        let first_ws_y = -self.workspace_render_idx() * ws_height_with_gap;
        let first_ws_y = round_logical_in_physical(scale, first_ws_y);

        let progress = self
            .overview_progress_value()
            .unwrap_or_default()
            .clamp(0., 1.);
        let overview_origins: Vec<_> = if self.options.overview.framed_workspaces && progress > 0. {
            let final_zoom = self.options.overview.zoom.clamp(0.0001, 0.75);
            let usable = self.overview_usable_area(1.);
            let viewport =
                usable.loc.x + OVERVIEW_INSET..usable.loc.x + usable.size.w - OVERVIEW_INSET;
            self.workspaces
                .iter()
                .enumerate()
                .map(|(idx, ws)| {
                    let top = static_offset.y + first_ws_y + idx as f64 * ws_height_with_gap;
                    if top + ws_size.h < 0. || top > self.view_size.h {
                        return None;
                    }

                    let positions = self.floating_overview_positions(ws, 1.);
                    let extent = visible_extent(ws.tiles_with_overview_positions(&positions))?;
                    fit_overview_strip(extent, &viewport, zoom, final_zoom)
                })
                .collect()
        } else {
            Vec::new()
        };

        // Return position for one-past-last workspace too.
        (0..=self.workspaces.len()).map(move |idx| {
            let y = first_ws_y + idx as f64 * ws_height_with_gap;
            let mut loc = Point::from((0., y)) + static_offset;
            if let Some(Some(origin)) = overview_origins.get(idx) {
                loc.x += (origin - loc.x) * progress;
            }

            // Even though all components that go into loc are rounded to physical pixels, the
            // floating point addition may lose precision. This can result for example in the
            // current workspace having y = 0.0000000000002 and thus missing pointer hits at the
            // monitor edge with y = 0. So, post-round the location too.
            let loc = loc.to_physical_precise_round(scale).to_logical(scale);

            Rectangle::new(loc, ws_size)
        })
    }

    pub fn workspaces_with_render_geo_cull(
        &self,
        cull: bool,
    ) -> impl Iterator<Item = (&Workspace<W>, Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| !cull || geo.intersection(output_geo).is_some())
    }

    pub fn workspaces_with_render_geo(
        &self,
    ) -> impl Iterator<Item = (&Workspace<W>, Rectangle<f64, Logical>)> {
        self.workspaces_with_render_geo_cull(true)
    }

    pub fn workspaces_with_render_geo_idx(
        &self,
    ) -> impl Iterator<Item = ((usize, &Workspace<W>), Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter().enumerate(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| geo.intersection(output_geo).is_some())
    }

    pub fn workspaces_with_render_geo_mut(
        &mut self,
        cull: bool,
    ) -> impl Iterator<Item = (&mut Workspace<W>, Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter_mut(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| !cull || geo.intersection(output_geo).is_some())
    }

    pub fn workspace_under(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&Workspace<W>, Rectangle<f64, Logical>)> {
        let (ws, geo) = self.workspaces_with_render_geo().find_map(|(ws, geo)| {
            // Extend width to entire output.
            let loc = Point::from((0., geo.loc.y));
            let size = Size::from((self.view_size.w, geo.size.h));
            let bounds = Rectangle::new(loc, size);

            bounds.contains(pos_within_output).then_some((ws, geo))
        })?;
        Some((ws, geo))
    }

    pub fn workspace_under_narrow(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<&Workspace<W>> {
        if self.has_framed_overview() && !self.overview_content().contains(pos_within_output) {
            return None;
        }
        self.workspaces_with_render_geo_idx()
            .find_map(|((index, ws), geo)| {
                let geo = if self.has_framed_overview() {
                    self.overview_frame_geometry(ws, index, geo)?
                } else {
                    geo
                };
                geo.contains(pos_within_output).then_some(ws)
            })
    }

    pub fn window_under(&self, pos_within_output: Point<f64, Logical>) -> Option<(&W, HitType)> {
        let (ws, geo) = self.workspace_under(pos_within_output)?;

        if self.overview_progress.is_some() {
            if self.options.overview.framed_workspaces
                && !self.overview_content().contains(pos_within_output)
            {
                return None;
            }
            let zoom = self.overview_zoom();
            let pos_within_workspace = (pos_within_output - geo.loc).downscale(zoom);
            let positions = self.floating_overview_positions(
                ws,
                self.overview_progress_value().unwrap_or_default(),
            );
            let (win, hit) = ws.window_under_in_overview(pos_within_workspace, &positions)?;
            // During the overview animation, we cannot do input hits because we cannot really
            // represent scaled windows properly.
            Some((win, hit.to_activate()))
        } else {
            let (win, hit) = ws.window_under(pos_within_output - geo.loc)?;
            if self.scrolling_clip_excludes(ws, geo.loc, pos_within_output)
                && !ws.is_floating(win.id())
            {
                return None;
            }
            Some((win, hit.offset_win_pos(geo.loc)))
        }
    }

    pub fn resize_edges_under(&self, pos_within_output: Point<f64, Logical>) -> Option<ResizeEdge> {
        if self.overview_progress.is_some() {
            return None;
        }

        let (ws, geo) = self.workspace_under(pos_within_output)?;
        if self.scrolling_clip_excludes(ws, geo.loc, pos_within_output) {
            self.window_under(pos_within_output)?;
        }
        ws.resize_edges_under(pos_within_output - geo.loc)
    }

    /// Clip normal tiled windows at exclusive side panels, while leaving the
    /// floating layer and maximized or full-screen window behavior intact.
    fn scroll_clip(&self, ws: &Workspace<W>) -> Option<Range<f64>> {
        if !ws.options.layout.clip_scroll_to_work_area
            || self.overview_progress.is_some()
            || !ws
                .scrolling()
                .active_window()
                .is_some_and(|window| window.sizing_mode().is_normal())
        {
            return None;
        }
        Some(self.scroll_view_bounds())
    }

    fn scroll_view_bounds(&self) -> Range<f64> {
        let area = self.working_area;
        area.loc.x..(area.loc.x + area.size.w)
    }

    fn scrolling_visible_span(&self, ws: &Workspace<W>) -> Option<Range<f64>> {
        let bounds = self.scroll_clip(ws)?;
        if ws.id() == self.active_workspace_ref().id() {
            return Some(Self::exclude_narrow_peeks(bounds, &self.scroll_cues()));
        }
        Some(bounds)
    }

    fn navigation_strip_top(&self) -> Option<f64> {
        let area = self.working_area;
        navigation_strip_fits(area).then_some(area.loc.y)
    }

    fn scrolling_clip_excludes(
        &self,
        ws: &Workspace<W>,
        workspace_origin: Point<f64, Logical>,
        location: Point<f64, Logical>,
    ) -> bool {
        self.scrolling_visible_span(ws).is_some_and(|bounds| {
            !bounds.contains(&location.x)
                || self.navigation_strip_top().is_some_and(|top| {
                    location.y - workspace_origin.y < top + NAVIGATION_STRIP_HEIGHT
                })
        })
    }

    fn exclude_narrow_peeks(
        mut bounds: Range<f64>,
        [left, right]: &[Option<ScrollCue<'_, W>>; 2],
    ) -> Range<f64> {
        if let Some(edge) = left.as_ref().and_then(|cue| cue.clipped_column_edge) {
            bounds.start = bounds.start.max(edge);
        }
        if let Some(edge) = right.as_ref().and_then(|cue| cue.clipped_column_edge) {
            bounds.end = bounds.end.min(edge);
        }
        bounds
    }

    fn scroll_cues(&self) -> [Option<ScrollCue<'_, W>>; 2] {
        let ws = self.active_workspace_ref();
        if self.workspace_switch.is_some() {
            return [None, None];
        }
        let Some(bounds) = self.scroll_clip(ws) else {
            return [None, None];
        };
        self.scroll_cues_for(ws, &bounds)
    }

    fn scroll_cues_for<'a>(
        &self,
        ws: &'a Workspace<W>,
        bounds: &Range<f64>,
    ) -> [Option<ScrollCue<'a, W>>; 2] {
        if ws.floating_is_active() {
            return [None, None];
        }
        let width = ((bounds.end - bounds.start) / 4.).floor().min(230.);
        let height = SCROLL_BUTTON_HEIGHT;
        if width < 32. || self.working_area.size.h < height {
            return [None, None];
        }
        let scrolling = ws.scrolling();
        let Some(active) = scrolling.active_column() else {
            return [None, None];
        };
        let active_x = scrolling
            .columns_with_render_positions()
            .find_map(|(column, pos)| (column.id() == active.id()).then_some(pos.x));
        let tolerance = 1. / self.scale.fractional_scale();
        let columns = || {
            scrolling
                .columns_with_render_positions()
                .filter(|(column, _)| column.id() != active.id())
        };
        let left_columns = || columns().filter(|(_, pos)| pos.x < bounds.start - tolerance);
        let right_columns =
            || columns().filter(|(column, pos)| pos.x + column.width() > bounds.end + tolerance);
        let left = left_columns().max_by(|(_, a), (_, b)| a.x.total_cmp(&b.x));
        let right = right_columns().min_by(|(_, a), (_, b)| a.x.total_cmp(&b.x));
        let fully_visible = columns()
            .filter(|(column, pos)| {
                pos.x >= bounds.start - tolerance
                    && pos.x + column.width() <= bounds.end + tolerance
            })
            .fold(
                (
                    active_x.unwrap_or(bounds.start),
                    active_x.unwrap_or(bounds.end) + active.width(),
                ),
                |(start, end), (column, pos)| (start.min(pos.x), end.max(pos.x + column.width())),
            );
        let inset = 12.;
        let left_edge = bounds.start + inset;
        let right_edge = bounds.end - inset;
        let left_width =
            active_x.map_or(width, |x| (x - left_edge - inset).floor().clamp(32., width));
        let right_width = active_x.map_or(width, |x| {
            (right_edge - x - active.width() - inset)
                .floor()
                .clamp(32., width)
        });
        let card_progress = scrolling
            .distance_to_stationary_view()
            .map_or(0., |distance| (1. - distance / 62.).clamp(0., 1.).powi(2));
        let make_cue =
            |(column, pos): (&'a Column<W>, Point<f64, Logical>), x, width: f64, left| {
                let edge = if left { pos.x + column.width() } else { pos.x };
                let exposed = if left {
                    edge - bounds.start
                } else {
                    bounds.end - edge
                };
                let unclipped = active_x.is_some_and(|x| {
                    if left {
                        edge <= x
                    } else {
                        edge >= x + active.width()
                    }
                });
                let space = if left {
                    fully_visible.0 - x - inset
                } else {
                    right_edge - fully_visible.1 - inset
                };
                let width = width.min(space.floor().max(36.));
                // Keep the live strip during a gesture. Reveal the resting control
                // as the camera approaches its final position.
                let can_clip = card_progress > 0. && unclipped;
                let card = can_clip
                    && width >= 144.
                    && self.working_area.size.h >= SCROLL_CARD_HEIGHT
                    && exposed < column.width() * 0.60;
                // With the strip available, retract the last part of a clipped
                // column before its titlebar buttons become detached slivers.
                let peek_width = (column.width() * 0.30).min(168.);
                let strip_peek = self.navigation_strip_top().is_some()
                    && unclipped
                    && exposed > 0.
                    && exposed < peek_width;
                let clip_progress = if strip_peek {
                    let progress = ((peek_width - exposed) / (peek_width * 0.36))
                        .max(card_progress)
                        .min(1.);
                    if exposed * (1. - progress) < 64. {
                        1.
                    } else {
                        progress
                    }
                } else if card {
                    card_progress
                } else {
                    0.
                };
                let compact = !card && width < 102.;
                let width = if compact && card_progress == 1. {
                    ((bounds.end - bounds.start) / 4.).floor().clamp(36., 182.)
                } else {
                    width
                };
                let height = if card { SCROLL_CARD_HEIGHT } else { height };
                let available = self.working_area.size.h - height;
                let y = self.working_area.loc.y
                    + if card {
                        available * 0.30
                    } else if compact {
                        (available - inset).max(0.)
                    } else {
                        available.min(72.)
                    };
                let rect = Rectangle::new(
                    Point::from((
                        match (left, card) {
                            (true, true) => bounds.start,
                            (false, true) => bounds.end - width,
                            (true, false) => x,
                            (false, false) => right_edge - width,
                        },
                        y,
                    )),
                    Size::from((width, height)),
                );
                let additional_top = y + height + 10.;
                let has_space = additional_top + SCROLL_CARD_HEIGHT
                    <= self.working_area.loc.y + self.working_area.size.h;
                let additional = if card && has_space {
                    let id = column.id();
                    let next = if left {
                        left_columns()
                            .filter(|(column, _)| column.id() != id)
                            .max_by(|(_, a), (_, b)| a.x.total_cmp(&b.x))
                    } else {
                        right_columns()
                            .filter(|(column, _)| column.id() != id)
                            .min_by(|(_, a), (_, b)| a.x.total_cmp(&b.x))
                    };
                    next.map(|(column, _)| {
                        (
                            Rectangle::new(
                                Point::from((rect.loc.x, additional_top)),
                                Size::from((width, SCROLL_CARD_HEIGHT)),
                            ),
                            column.active_window(),
                        )
                    })
                } else {
                    None
                };
                ScrollCue {
                    rect,
                    destination: column.active_window(),
                    clipped_column_edge: (clip_progress > 0.).then(|| {
                        let boundary = if left { bounds.start } else { bounds.end };
                        boundary + (edge - boundary) * clip_progress
                    }),
                    opacity: if card { card_progress as f32 } else { 1. },
                    offscreen_columns: if left {
                        left_columns().count()
                    } else {
                        right_columns().count()
                    },
                    additional,
                }
            };
        [
            left.map(|column| make_cue(column, left_edge, left_width, true)),
            right.map(|column| make_cue(column, right_edge - right_width, right_width, false)),
        ]
    }

    /// Normal tiled windows leave a row for the strip, so the same controls
    /// remain available in each position without painting over a client surface.
    fn navigation_strip(&self, decorate: bool) -> Option<NavigationStrip<'_, W>> {
        let ws = self.active_workspace_ref();
        if self.workspace_switch.is_some()
            || self.overview_progress.is_some()
            || !ws.options.layout.clip_scroll_to_work_area
        {
            return None;
        }
        let top = self.navigation_strip_top()?;
        let scrolling = ws.scrolling();
        let bounds = self.scroll_clip(ws).or_else(|| {
            let area = self.working_area;
            scrolling
                .is_empty()
                .then_some(area.loc.x..area.loc.x + area.size.w)
        })?;
        let active = ws.active_window()?;
        let pixel = 1. / self.scale.fractional_scale();
        let moving = scrolling
            .remaining_view_movement()
            .is_some_and(|movement| movement.abs() > 2. * pixel);
        let positions: HashMap<_, _> = scrolling
            .columns_with_render_positions()
            .map(|(column, position)| (column.id(), position.x))
            .collect();
        let cues = self.scroll_cues_for(ws, &bounds);
        let visible = Self::exclude_narrow_peeks(bounds.clone(), &cues);
        let mut windows = scrolling
            .columns()
            .map(|column| {
                let window = column.active_window();
                let x = positions[&column.id()];
                let clipped_left = (visible.start - x).max(0.);
                let clipped_right = (x + column.width() - visible.end).max(0.);
                let fully_visible = clipped_left.max(clipped_right) <= pixel;
                let direction = (!fully_visible && window.id() != active.id())
                    .then_some(clipped_left >= clipped_right);
                (window, direction, fully_visible, false, column.width(), x)
            })
            .collect::<Vec<_>>();
        let mut floating = ws
            .floating()
            .tiles_with_render_positions()
            .collect::<Vec<_>>();
        floating.sort_by(|(_, a), (_, b)| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
        windows.extend(floating.iter().map(|(tile, position)| {
            (
                tile.window(),
                None,
                false,
                true,
                tile.tile_size().w,
                position.x,
            )
        }));
        if windows.len() == 1 && floating.is_empty() {
            return None;
        }
        let has_offscreen_windows = windows.iter().any(|(_, direction, ..)| direction.is_some());

        let active_index = windows
            .iter()
            .position(|(window, ..)| window.id() == active.id())?;
        let gap = 8.;
        let inset = 16.;
        let available = bounds.end - bounds.start - 2. * inset;
        let capacity = ((available + gap) / 160.).floor() as usize;
        let count = windows
            .len()
            .min(capacity.max(1))
            .min(MAX_NAVIGATION_TITLES);
        let action_width = 188.;
        let action_gap = 16.;
        let floating_visible = ws.is_floating_visible() && !floating.is_empty();
        let layer_destination = match (scrolling.active_window(), ws.floating().active_window()) {
            (Some(tiled), Some(floating))
                if available >= action_width + action_gap + (150. + gap) * count as f64 =>
            {
                Some(if floating_visible { tiled } else { floating })
            }
            _ => None,
        };
        let action_space = layer_destination.map_or(0., |_| action_width + action_gap);
        let first = active_index
            .saturating_sub(count / 2)
            .min(windows.len() - count);
        let windows = &windows[first..first + count];
        let gaps = (count - 1) as f64 * gap;
        let button_space = (available - action_space - gaps).min(194. * count as f64);
        let widest_window = windows
            .iter()
            .map(|(_, _, _, _, width, _)| *width)
            .fold(1., f64::max);
        let weight = |width: f64| width.max(widest_window * 0.55);
        let total_weight: f64 = windows
            .iter()
            .map(|(_, _, _, _, width, _)| weight(*width))
            .sum();
        let tabs_width = button_space + gaps;
        let tray_width = tabs_width + action_space;
        let start = (bounds.start + bounds.end - tray_width) / 2.;
        let y = top + (NAVIGATION_STRIP_HEIGHT - STRIP_BUTTON_HEIGHT) / 2.;
        let uncovered = |rect: Rectangle<f64, Logical>| {
            let covered = floating_visible
                && floating.iter().any(|(tile, position)| {
                    rect.intersection(Rectangle::new(*position, tile.tile_size()))
                        .is_some()
                });
            (!covered).then_some(rect)
        };
        let button = |x, width| {
            uncovered(Rectangle::new(
                Point::from((x, y)),
                Size::from((width, STRIP_BUTTON_HEIGHT)),
            ))
        };
        let mut next_x = start;
        let mut motion_extent = f64::INFINITY..f64::NEG_INFINITY;
        let titles: Vec<_> = windows
            .iter()
            .filter_map(
                |&(window, direction, fully_visible, floating, window_width, column_x)| {
                    let width = button_space * weight(window_width) / total_weight;
                    let x = next_x;
                    next_x += width + gap;
                    let mut rect = button(x, width)?;
                    let compact =
                        !floating && (!floating_visible || layer_destination.is_some() || moving);
                    if compact {
                        rect.size.h = STRIP_GROUP_BUTTON_HEIGHT;
                    }
                    if decorate && moving && !floating {
                        let left = (visible.start - column_x).max(0.);
                        let right = (visible.end - column_x).min(window_width);
                        if right > left && window_width > 0. {
                            motion_extent.start =
                                motion_extent.start.min(x + width * left / window_width);
                            motion_extent.end =
                                motion_extent.end.max(x + width * right / window_width);
                        }
                    }
                    let mut visible_after = Vec::new();
                    let mut companions = Vec::new();
                    if decorate && !moving && direction.is_some() {
                        let columns = scrolling.columns_after_activating(window.id());
                        for (column, x) in columns.into_iter().flatten() {
                            let width = column.width();
                            if x < bounds.start - pixel || x + width > bounds.end + pixel {
                                continue;
                            }
                            let neighbor = column.active_window();
                            visible_after.push(neighbor);
                            let current = positions[&column.id()];
                            let already_visible = current >= visible.start - pixel
                                && current + width <= visible.end + pixel;
                            if !already_visible && neighbor.id() != window.id() {
                                companions.push(neighbor);
                            }
                        }
                    }
                    Some(NavigationTitle {
                        rect,
                        window,
                        companions,
                        visible_after,
                        active: window.id() == active.id(),
                        direction,
                        fully_visible,
                        floating,
                    })
                },
            )
            .collect();
        let layer_action = layer_destination.and_then(|window| {
            button(start + tabs_width + action_gap, action_width).map(|rect| (rect, window))
        });
        let viewport_mark = |span: Range<f64>| {
            uncovered(Rectangle::new(
                Point::from((span.start, y + STRIP_BUTTON_HEIGHT)),
                Size::from((span.end - span.start, 2.)),
            ))
        };
        let tray = uncovered(Rectangle::new(
            Point::from((start - STRIP_TRAY_PADDING, top + 4.)),
            Size::from((tray_width + 2. * STRIP_TRAY_PADDING, STRIP_TRAY_HEIGHT)),
        ));
        let edge_actions = if floating.is_empty() && has_offscreen_windows {
            [true, false].map(|left| {
                let tray = tray?;
                let mut targets = titles.iter().filter(|title| title.direction == Some(left));
                let target = if left { targets.last() } else { targets.next() };
                let width = 30.;
                let x = if left {
                    tray.loc.x - 6. - width
                } else {
                    tray.loc.x + tray.size.w + 6.
                };
                if x < bounds.start || x + width > bounds.end {
                    return None;
                }
                Some(NavigationArrow {
                    rect: button(x, width)?,
                    target: target.map(|title| title.window),
                })
            })
        } else {
            [None, None]
        };
        let viewports = if !decorate {
            Vec::new()
        } else if moving {
            (!motion_extent.is_empty())
                .then(|| viewport_mark(motion_extent))
                .flatten()
                .into_iter()
                .collect()
        } else {
            let mut spans: Vec<Range<f64>> = Vec::new();
            for title in titles.iter().filter(|title| title.fully_visible) {
                let start = title.rect.loc.x;
                let end = start + title.rect.size.w;
                if let Some(span) = spans
                    .last_mut()
                    .filter(|span| start <= span.end + gap + 0.001)
                {
                    span.end = end;
                } else {
                    spans.push(start..end);
                }
            }
            spans.into_iter().filter_map(viewport_mark).collect()
        };
        Some(NavigationStrip {
            tray,
            viewports,
            moving,
            edge_actions,
            titles,
            layer_action,
        })
    }

    pub fn select_overview_preview(&mut self, id: &W::Id) -> bool {
        if !self.overview_open
            || !self.has_framed_overview()
            || self.overview_progress_value().is_none_or(|p| p < 0.82)
            || self.overview_pointer_selection.as_ref() == Some(id)
            || !self
                .active_workspace_ref()
                .windows()
                .any(|window| window.id() == id)
        {
            return false;
        }
        self.overview_pointer_selection = Some(id.clone());
        true
    }

    pub(super) fn take_overview_pointer_selection(&mut self) -> Option<&W> {
        let id = self.overview_pointer_selection.take()?;
        self.active_workspace_ref()
            .windows()
            .find(|window| window.id() == &id)
    }

    fn overview_selected_window(&self) -> Option<&W> {
        let ws = self.active_workspace_ref();
        self.overview_pointer_selection
            .as_ref()
            .and_then(|id| ws.windows().find(|window| window.id() == id))
            .or_else(|| ws.active_window())
    }

    fn overview_command(&self) -> Option<(Rectangle<f64, Logical>, &W)> {
        if !self.overview_open || !self.has_framed_overview() {
            return None;
        }

        let progress = self.overview_progress_value()?;
        if progress < 0.82 {
            return None;
        }

        let window = self.overview_selected_window()?;
        let ((_, ws), geo) = self
            .workspaces_with_render_geo_idx()
            .find(|((idx, _), _)| *idx == self.active_workspace_idx)?;
        let zoom = self.overview_zoom();
        let positions = self.floating_overview_positions(ws, progress);
        let previews = || {
            ws.tiles_with_overview_positions(&positions)
                .filter_map(|(tile, pos, visible)| {
                    visible.then_some((
                        tile.window().id(),
                        Rectangle::new(geo.loc + pos.upscale(zoom), tile.tile_size().upscale(zoom)),
                    ))
                })
        };
        let (_, preview) = previews().find(|(id, _)| *id == window.id())?;
        let area = self.overview_content();
        let bottom = previews()
            .map(|(_, preview)| preview.loc.y + preview.size.h)
            .reduce(f64::max)?;
        let width = 216.;
        let rect = Rectangle::new(
            Point::from((area.loc.x + (area.size.w - width) / 2., bottom + 17.)),
            Size::from((width, OVERVIEW_CONTROL_HEIGHT)),
        );
        let fits = |part: Rectangle<f64, Logical>| {
            part.loc.x >= area.loc.x
                && part.loc.y >= area.loc.y
                && part.loc.x + part.size.w <= area.loc.x + area.size.w
                && part.loc.y + part.size.h <= area.loc.y + area.size.h
        };
        let clear = fits(rect)
            && fits(preview)
            && previews().all(|(_, preview)| preview.intersection(rect).is_none());
        clear.then_some((rect, window))
    }

    pub fn navigation_window_under(&self, position: Point<f64, Logical>) -> Option<&W> {
        if self.overview_open {
            return self
                .overview_command()
                .filter(|(rect, _)| rect.contains(position))
                .map(|(_, window)| window);
        }

        if let Some(strip) = self.navigation_strip(false) {
            return strip
                .titles
                .into_iter()
                .find(|title| title.rect.contains(position))
                .map(|title| title.window)
                .or_else(|| {
                    strip
                        .edge_actions
                        .into_iter()
                        .flatten()
                        .find(|arrow| arrow.rect.contains(position))
                        .and_then(|arrow| arrow.target)
                })
                .or_else(|| {
                    strip
                        .layer_action
                        .filter(|(rect, _)| rect.contains(position))
                        .map(|(_, window)| window)
                });
        }
        self.scroll_cues()
            .into_iter()
            .flatten()
            .filter(|cue| cue.opacity > 0.5)
            .find_map(|cue| {
                if cue.rect.contains(position) {
                    Some(cue.destination)
                } else {
                    cue.additional
                        .filter(|(rect, _)| rect.contains(position))
                        .map(|(_, window)| window)
                }
            })
    }

    pub fn window_has_navigation_caption(&self, id: &W::Id) -> bool {
        self.has_framed_overview()
            || self
                .navigation_strip(false)
                .is_some_and(|strip| strip.titles.iter().any(|title| title.window.id() == id))
            || self.scroll_cues().into_iter().flatten().any(|cue| {
                cue.destination.id() == id
                    || cue.additional.is_some_and(|(_, window)| window.id() == id)
            })
    }

    fn render_scroll_preview<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
        rect: Rectangle<f64, Logical>,
        destination: &W,
        opacity: f32,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        if opacity < 1. || target.should_block_out(destination.rules().block_out_from) {
            return;
        }
        let source = destination.size().to_f64();
        if source.w <= 0. || source.h <= 0. {
            return;
        }
        let width = rect.size.w;
        let height = rect.size.h;
        let scale = self.scale.fractional_scale();
        let zoom = (width / source.w).min(1.);
        let crop = Rectangle::from_size(Size::from((
            (width / zoom * scale).floor() as i32,
            (height / zoom * scale).floor() as i32,
        )));
        let location = rect.loc.to_physical_precise_round(scale);
        let ctx = RenderCtx {
            renderer,
            target,
            xray: None,
        };
        destination.render_normal(ctx, Point::default(), scale.into(), 1., &mut |elem| {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, crop) {
                let elem = MonitorInnerRenderElement::WindowPreview(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), zoom);
                push(RelocateRenderElement::from_element(
                    elem,
                    location,
                    Relocate::Relative,
                ));
            }
        });
    }

    fn render_navigation_forecast<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
        title: &NavigationTitle<'_, W>,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) -> bool {
        let ws = self.active_workspace_ref();
        let area = self.working_area;
        let top = title.rect.loc.y + STRIP_BUTTON_HEIGHT + 8.;
        let Some(clip) = self.scroll_clip(ws) else {
            return false;
        };
        let windows = title.visible_after.as_slice();
        let width = (188. * windows.len() as f64)
            .clamp(236., 480.)
            .min(clip.end - clip.start - 20.);
        if width < 210. || top + STRIP_FORECAST_HEIGHT > area.loc.y + area.size.h {
            return false;
        }
        let padding = 10.;
        let gap = 8.;
        let left = (title.rect.loc.x + (title.rect.size.w - width) / 2.)
            .clamp(clip.start + padding, clip.end - width - padding);
        let accent = ws.options.layout.border.active_color;
        let surface = caption_background(accent, ws.options.layout.background_color, false);
        let scale = self.scale.fractional_scale();
        let available = width - 2. * padding - gap * windows.len().saturating_sub(1) as f64;
        let widest = windows
            .iter()
            .map(|window| window.size().w)
            .max()
            .unwrap_or(1);
        let weight = |window: &W| f64::from(window.size().w.max(1)).max(f64::from(widest) * 0.55);
        let total: f64 = windows.iter().map(|window| weight(*window)).sum();
        let names = windows
            .iter()
            .map(|window| navigation_title(*window, target, "Window"))
            .collect::<Vec<_>>();
        let mut x = left + padding;
        for (index, (window, label)) in windows.iter().zip(&self.strip_forecast_titles).enumerate()
        {
            let width = available * weight(*window) / total;
            let rect = Rectangle::new(Point::from((x, top + 32.)), Size::from((width, 99.)));
            let focused = window.id() == title.window.id();
            if focused {
                ws.overview_caption.render_selection(
                    renderer,
                    rect,
                    accent,
                    1.,
                    scale,
                    &mut |outline| {
                        let element = MonitorInnerRenderElement::Decoration(outline);
                        push(RelocateRenderElement::from_element(
                            RescaleRenderElement::from_element(element, Point::default(), 1.),
                            Point::default(),
                            Relocate::Relative,
                        ));
                    },
                );
            }
            let name = &names[index];
            let suffix = window_title_suffix(name);
            let short = names
                .iter()
                .enumerate()
                .all(|(other, name)| other == index || window_title_suffix(name) != suffix);
            if let Some(caption) = label.render(
                renderer.as_gles_renderer(),
                compact_window_title(name, short).to_owned(),
                if focused {
                    accent
                } else {
                    tint_caption(surface, accent)
                },
                LabelKind::Preview {
                    badge: (focused && width >= 96.).then_some("Focus"),
                    current: false,
                },
                width,
                scale,
                rect.loc,
                1.,
            ) {
                let element = MonitorInnerRenderElement::Caption(caption);
                push(RelocateRenderElement::from_element(
                    RescaleRenderElement::from_element(element, Point::default(), 1.),
                    Point::default(),
                    Relocate::Relative,
                ));
            }
            self.render_scroll_preview(
                renderer,
                target,
                Rectangle::new(rect.loc + Point::from((0., 22.)), Size::from((width, 77.))),
                *window,
                1.,
                push,
            );
            x += width + gap;
        }
        if let Some(caption) = self.strip_forecast_label.render(
            renderer.as_gles_renderer(),
            "In view after switch".into(),
            surface,
            LabelKind::StripForecast,
            width,
            scale,
            Point::from((left, top)),
            1.,
        ) {
            let element = MonitorInnerRenderElement::Caption(caption);
            push(RelocateRenderElement::from_element(
                RescaleRenderElement::from_element(element, Point::default(), 1.),
                Point::default(),
                Relocate::Relative,
            ));
        }
        true
    }

    fn render_scroll_cues<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
        pointer: Option<Point<f64, Logical>>,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        if let Some(strip) = self.navigation_strip(true) {
            let ws = self.active_workspace_ref();
            let accent = ws.options.layout.border.active_color;
            let surface = caption_background(accent, ws.options.layout.background_color, false);
            let direction_background = tint_caption(tint_caption(surface, accent), accent);
            let scale = self.scale.fractional_scale();
            let has_layer_action = strip.layer_action.is_some();
            let floating_visible = ws.is_floating_visible() && !ws.floating().is_empty();
            let moving = strip.moving;
            let edge_target = pointer.and_then(|point| {
                strip
                    .edge_actions
                    .iter()
                    .flatten()
                    .find(|arrow| arrow.rect.contains(point))
                    .and_then(|arrow| arrow.target)
                    .map(|window| window.id())
            });
            let forecast = pointer.and_then(|point| {
                strip.titles.iter().find(|title| {
                    !moving
                        && !floating_visible
                        && title.direction.is_some()
                        && !title.visible_after.is_empty()
                        && title.visible_after.len() <= self.strip_forecast_titles.len()
                        && title
                            .visible_after
                            .iter()
                            .any(|window| window.id() == title.window.id())
                        && (title.rect.contains(point) || edge_target == Some(title.window.id()))
                })
            });
            let forecast = forecast
                .filter(|title| self.render_navigation_forecast(renderer, target, title, push));
            let ink = caption_ink(surface);
            let gray = (ink.r + ink.g + ink.b) / 3.;
            let viewport_color = tint_caption(
                tint_caption(Color::new_unpremul(gray, gray, gray, 1.), surface),
                surface,
            );
            let marks = strip.viewports.iter().flat_map(|line| {
                let cap_width = line.size.w.min(2.);
                let cap = Rectangle::new(
                    line.loc - Point::from((0., 3.)),
                    Size::from((cap_width, 3.)),
                );
                [
                    *line,
                    cap,
                    Rectangle::new(
                        cap.loc + Point::from((line.size.w - cap_width, 0.)),
                        cap.size,
                    ),
                ]
            });
            for (rect, buffer) in marks.zip(&self.strip_viewport_marks) {
                let mut buffer = buffer.borrow_mut();
                buffer.update(rect.size, viewport_color);
                let element =
                    SolidColorRenderElement::from_buffer(&buffer, rect.loc, 1., Kind::Unspecified);
                let element = MonitorInnerRenderElement::SolidColor(element);
                push(RelocateRenderElement::from_element(
                    RescaleRenderElement::from_element(element, Point::default(), 1.),
                    Point::default(),
                    Relocate::Relative,
                ));
            }
            let mut draw = |label| {
                let elem = MonitorInnerRenderElement::Caption(label);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                push(RelocateRenderElement::from_element(
                    elem,
                    Point::default(),
                    Relocate::Relative,
                ));
            };
            let titles: Vec<_> = strip
                .titles
                .iter()
                .map(|title| navigation_title(title.window, target, "Window"))
                .collect();
            let aliases: Vec<_> = titles
                .iter()
                .enumerate()
                .map(|(index, title)| {
                    let suffix = window_title_suffix(title);
                    let distinct = titles.iter().enumerate().all(|(other, title)| {
                        other == index || window_title_suffix(title) != suffix
                    });
                    compact_window_title(title, distinct)
                })
                .collect();
            let display_title = |index: usize| {
                let alias = aliases[index];
                let distinct = aliases
                    .iter()
                    .enumerate()
                    .all(|(other, name)| other == index || *name != alias);
                if distinct {
                    alias
                } else {
                    titles[index].as_str()
                }
            };
            let mut hover_status = None;
            for (index, (title, label)) in strip.titles.iter().zip(&self.strip_labels).enumerate() {
                let hovered = pointer.is_some_and(|point| title.rect.contains(point));
                let referenced = edge_target == Some(title.window.id());
                let offscreen = title.direction.is_some();
                let compact = title.rect.size.h == STRIP_GROUP_BUTTON_HEIGHT;
                let has_forecast =
                    forecast.is_some_and(|forecast| forecast.window.id() == title.window.id());
                let status = if title.floating {
                    Some(if !floating_visible {
                        "Hidden"
                    } else {
                        "Floating"
                    })
                } else if !compact
                    && !has_layer_action
                    && !ws.floating().is_empty()
                    && floating_visible
                {
                    Some("Hides floating windows")
                } else {
                    None
                };
                let name = display_title(index);
                let focus = if title.active {
                    FocusMarker::Current
                } else {
                    FocusMarker::None
                };
                let background = if title.active {
                    accent
                } else if offscreen && (hovered || referenced) {
                    tint_caption(direction_background, accent)
                } else if offscreen || hovered || referenced {
                    direction_background
                } else if !title.floating {
                    tint_caption(surface, accent)
                } else {
                    surface
                };
                let kind = LabelKind::StripButton {
                    status,
                    outlined: !title.active,
                    hovered: hovered && !moving,
                    focus,
                    direction: title.direction,
                    compact,
                };
                let companion_hint = if !has_forecast && (hovered || referenced) {
                    match title.companions.as_slice() {
                        [] => None,
                        [neighbor] => {
                            let name = strip
                                .titles
                                .iter()
                                .position(|title| title.window.id() == neighbor.id())
                                .map(|index| display_title(index).to_owned())
                                .unwrap_or_else(|| navigation_title(*neighbor, target, "window"));
                            Some(format!("Also reveals {name}"))
                        }
                        companions => Some(format!("Also reveals {} more tiles", companions.len())),
                    }
                } else {
                    None
                };
                if let Some(hint) = companion_hint {
                    let mut rect = title.rect;
                    rect.loc.y += STRIP_BUTTON_HEIGHT + 4.;
                    rect.size.h = STRIP_STATUS_HEIGHT;
                    hover_status = Some((rect, hint));
                }
                if let Some(label) = label.render(
                    renderer.as_gles_renderer(),
                    name.to_owned(),
                    background,
                    kind,
                    title.rect.size.w - 6.,
                    scale,
                    title.rect.loc + Point::from((3., 0.)),
                    1.,
                ) {
                    draw(label);
                }
            }
            for (index, (edge, label)) in strip
                .edge_actions
                .iter()
                .zip(&self.strip_edge_labels)
                .enumerate()
            {
                let Some(arrow) = edge else { continue };
                let enabled = arrow.target.is_some();
                let rect = arrow.rect;
                let hovered = enabled && pointer.is_some_and(|point| rect.contains(point));
                if let Some(arrow) = label.render(
                    renderer.as_gles_renderer(),
                    String::new(),
                    if !enabled {
                        surface
                    } else if hovered {
                        tint_caption(direction_background, accent)
                    } else {
                        direction_background
                    },
                    LabelKind::ScrollWindow {
                        left: index == 0,
                        hovered: hovered && !moving,
                    },
                    rect.size.w,
                    scale,
                    rect.loc,
                    if enabled { 1. } else { 0.55 },
                ) {
                    draw(arrow);
                }
            }
            if let Some((rect, _)) = strip.layer_action {
                let hovered = pointer.is_some_and(|point| rect.contains(point));
                let text = if floating_visible {
                    "Hide floating windows"
                } else {
                    "Show floating windows"
                };
                if let Some(label) = self.layer_action_label.render(
                    renderer.as_gles_renderer(),
                    text.into(),
                    if hovered {
                        tint_caption(tint_caption(surface, accent), accent)
                    } else {
                        tint_caption(surface, accent)
                    },
                    LabelKind::LayerAction {
                        show: !floating_visible,
                    },
                    rect.size.w,
                    scale,
                    rect.loc,
                    1.,
                ) {
                    draw(label);
                }
            }
            if let Some((rect, text)) = hover_status {
                if let Some(caption) = self.strip_status_label.render(
                    renderer.as_gles_renderer(),
                    text,
                    surface,
                    LabelKind::StripStatus,
                    rect.size.w,
                    scale,
                    rect.loc,
                    1.,
                ) {
                    draw(caption);
                }
            }
            // Put the shared surface behind the window targets.
            if let Some(rect) = strip.tray {
                let background = Color { a: 0.73, ..surface };
                if let Some(tray) = self.strip_tray.render(
                    renderer.as_gles_renderer(),
                    String::new(),
                    background,
                    LabelKind::StripTray,
                    rect.size.w,
                    scale,
                    rect.loc,
                    1.,
                ) {
                    draw(tray);
                }
            }
            return;
        }
        let ws = self.active_workspace_ref();
        let accent = ws.options.layout.border.active_color;
        let surface = caption_background(accent, ws.options.layout.background_color, false);
        let color = tint_caption(surface, accent);
        let Some(bounds) = self.scroll_clip(ws) else {
            return;
        };
        let scale = self.scale.fractional_scale();
        for (index, cue) in self.scroll_cues().into_iter().enumerate() {
            let Some(cue) = cue else { continue };
            let left = index == 0;
            let label = &self.scroll_cue_labels[index];
            let card = cue.clipped_column_edge.is_some();
            if card {
                for (rect, window) in
                    std::iter::once((cue.rect, cue.destination)).chain(cue.additional)
                {
                    let thumbnail = Rectangle::new(
                        rect.loc + Point::from((10., 78.)),
                        Size::from((rect.size.w - 20., rect.size.h - 88.)),
                    );
                    self.render_scroll_preview(
                        renderer,
                        target,
                        thumbnail,
                        window,
                        cue.opacity,
                        push,
                    );
                }
            }
            let mut draw = |label| {
                let elem = MonitorInnerRenderElement::Caption(label);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                push(RelocateRenderElement::from_element(
                    elem,
                    Point::default(),
                    Relocate::Relative,
                ));
            };
            let fallback = if left {
                "Previous window"
            } else {
                "Next window"
            };
            let fallback = if card { "Window" } else { fallback };
            let title = if cue.rect.size.w < 102. {
                String::new()
            } else {
                navigation_title(cue.destination, target, fallback)
            };
            let text = if card {
                title
            } else if title.is_empty() {
                if left { "←" } else { "→" }.to_owned()
            } else if left {
                format!("←  {title}")
            } else {
                format!("{title}  →")
            };
            if let Some(label) = label.render(
                renderer.as_gles_renderer(),
                text,
                if pointer.is_some_and(|point| cue.rect.contains(point)) {
                    accent
                } else {
                    color
                },
                if card {
                    LabelKind::WindowNavigation {
                        left,
                        offset: 1,
                        more: if cue.additional.is_some() {
                            0
                        } else {
                            cue.offscreen_columns.saturating_sub(1)
                        },
                    }
                } else {
                    LabelKind::Button { align_right: !left }
                },
                cue.rect.size.w,
                scale,
                cue.rect.loc,
                cue.opacity,
            ) {
                draw(label);
            }
            if let Some((rect, window)) = cue.additional {
                if let Some(label) = self.additional_scroll_cue_labels[index].render(
                    renderer.as_gles_renderer(),
                    navigation_title(window, target, "Another window"),
                    if pointer.is_some_and(|point| rect.contains(point)) {
                        accent
                    } else {
                        color
                    },
                    LabelKind::WindowNavigation {
                        left,
                        offset: 2,
                        more: cue.offscreen_columns.saturating_sub(2),
                    },
                    rect.size.w,
                    scale,
                    rect.loc,
                    cue.opacity,
                ) {
                    draw(label);
                }
            }
            if card {
                continue;
            }
            let Some((tile, pos, _)) = ws
                .scrolling()
                .tiles_with_render_positions()
                .find(|(tile, _, visible)| *visible && tile.window().id() == cue.destination.id())
            else {
                continue;
            };
            let tile_right = pos.x + tile.tile_size().w;
            let boundary = if left { bounds.start } else { bounds.end };
            if !(pos.x..tile_right).contains(&boundary) {
                continue;
            }
            let width = if left {
                tile_right - boundary
            } else {
                boundary - pos.x
            };
            let width = width.floor().min(36.);
            let top = pos.y.max(self.working_area.loc.y);
            let bottom = (pos.y + tile.tile_size().h)
                .min(self.working_area.loc.y + self.working_area.size.h);
            let height = (bottom - top).floor().max(0.) as u32;
            if width < 2. || height == 0 {
                continue;
            }
            let x = if left { boundary } else { boundary - width };
            if let Some(fade) = self.scroll_edge_fades[index].render(
                renderer.as_gles_renderer(),
                String::new(),
                surface,
                LabelKind::EdgeFade {
                    height,
                    from_left: left,
                },
                width,
                scale,
                Point::from((x, top)),
                1.,
            ) {
                draw(fade);
            }
        }
    }

    pub(super) fn insert_position(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> (InsertWorkspace, Rectangle<f64, Logical>) {
        let mut iter = self.workspaces_with_render_geo_idx();

        let dummy = Rectangle::default();

        // Monitors always have at least one workspace.
        let ((idx, ws), geo) = iter.next().unwrap();

        // Check if above first.
        if pos_within_output.y < geo.loc.y {
            return (InsertWorkspace::NewAt(idx), dummy);
        }

        let contains = move |geo: Rectangle<f64, Logical>| {
            geo.loc.y <= pos_within_output.y && pos_within_output.y < geo.loc.y + geo.size.h
        };

        // Check first.
        if contains(geo) {
            return (InsertWorkspace::Existing(ws.id()), geo);
        }

        let mut last_geo = geo;
        let mut last_idx = idx;
        for ((idx, ws), geo) in iter {
            // Check gap above.
            let gap_loc = Point::from((last_geo.loc.x, last_geo.loc.y + last_geo.size.h));
            let gap_size = Size::from((geo.size.w, geo.loc.y - gap_loc.y));
            let gap_geo = Rectangle::new(gap_loc, gap_size);
            if contains(gap_geo) {
                return (InsertWorkspace::NewAt(idx), dummy);
            }

            // Check workspace itself.
            if contains(geo) {
                return (InsertWorkspace::Existing(ws.id()), geo);
            }

            last_geo = geo;
            last_idx = idx;
        }

        // Anything below.
        (InsertWorkspace::NewAt(last_idx + 1), dummy)
    }

    pub fn fullscreen_window_above_top_layer(&self) -> Option<&W> {
        // Render above the top layer only if the view is stationary.
        if self.workspace_switch.is_some() || self.overview_progress.is_some() {
            return None;
        }

        let ws = &self.workspaces[self.active_workspace_idx];
        ws.fullscreen_window_above_top_layer()
    }

    pub fn render_above_top_layer(&self) -> bool {
        self.fullscreen_window_above_top_layer().is_some()
    }

    pub fn render_insert_hint_between_workspaces<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        if self.options.layout.insert_hint.off {
            return;
        }
        let Some(render_loc) = self.insert_hint_render_loc else {
            return;
        };
        let InsertWorkspace::NewAt(_) = render_loc.workspace else {
            return;
        };

        self.insert_hint_element
            .render(renderer, render_loc.location, &mut |elem| {
                let elem = MonitorInnerRenderElement::Decoration(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                let elem =
                    RelocateRenderElement::from_element(elem, Point::default(), Relocate::Relative);
                push(elem);
            });
    }

    fn overview_content(&self) -> Rectangle<f64, Logical> {
        let progress = self
            .overview_progress_value()
            .unwrap_or_default()
            .clamp(0., 1.);
        self.overview_usable_area(progress)
    }

    /// Interpolate the crop from the entire output to the space clear of panels.
    fn overview_usable_area(&self, progress: f64) -> Rectangle<f64, Logical> {
        let area = self.working_area;
        let margin = OVERVIEW_FRAME_MARGIN.min(area.size.w / 2.);
        let loc = (area.loc + Point::from((margin, 0.))).upscale(progress);
        let usable_width = area.size.w - 2. * margin;
        let size = Size::from((
            self.view_size.w + (usable_width - self.view_size.w) * progress,
            self.view_size.h + (area.size.h - self.view_size.h) * progress,
        ));
        Rectangle::new(loc, size)
    }

    fn overview_crop(
        &self,
        geo: Rectangle<f64, Logical>,
        scale: f64,
        zoom: f64,
        height: i32,
    ) -> Rectangle<i32, Physical> {
        let area = self.overview_content();
        let near = (area.loc - geo.loc).downscale(zoom).upscale(scale);
        let far = near + area.size.upscale(scale / zoom).to_point();
        let left = near.x.ceil() as i32;
        let right = far.x.floor() as i32;
        let top = (near.y.ceil() as i32).clamp(0, height);
        let bottom = (far.y.floor() as i32).clamp(top, height);
        Rectangle::new(
            Point::from((left, top)),
            Size::from((right.saturating_sub(left).max(0), bottom - top)),
        )
    }

    fn desktop_window_groups(
        &self,
        ws: &Workspace<W>,
        geo: Rectangle<f64, Logical>,
    ) -> Option<[Range<f64>; 3]> {
        if !ws.floating().is_empty() {
            return None;
        }
        let tiles = || ws.scrolling().tiles_with_render_positions();
        let extent = visible_extent(tiles())?;
        let view = ws.working_area();
        let bounds = if ws.options.layout.clip_scroll_to_work_area {
            let bounds = self.scroll_view_bounds();
            Self::exclude_narrow_peeks(bounds.clone(), &self.scroll_cues_for(ws, &bounds))
        } else {
            view.loc.x..view.loc.x + view.size.w
        };
        let pixel = 1. / self.scale.fractional_scale();
        let visible = visible_extent(tiles().filter(|(tile, pos, _)| {
            pos.x < bounds.end - pixel && pos.x + tile.tile_size().w > bounds.start + pixel
        }))?;
        if visible == extent {
            return None;
        }
        let area = self.overview_content();
        let left = area.loc.x;
        let right = area.loc.x + area.size.w;
        let zoom = self.overview_zoom();
        let project = |range: Range<f64>| {
            let x = |x: f64| (geo.loc.x + zoom * x).clamp(left, right);
            x(range.start)..x(range.end)
        };
        let visible = project(visible);
        (visible.end - visible.start >= pixel).then(|| {
            let before = visible_extent(
                tiles().filter(|(tile, pos, _)| pos.x + tile.tile_size().w <= bounds.start + pixel),
            )
            .map_or(visible.start..visible.start, project);
            let after = visible_extent(tiles().filter(|(_, pos, _)| pos.x >= bounds.end - pixel))
                .map_or(visible.end..visible.end, project);
            [before, visible, after]
        })
    }

    fn overview_groups(
        &self,
        ws: &Workspace<W>,
        geo: Rectangle<f64, Logical>,
        progress: f64,
    ) -> Option<[Option<OverviewGroup>; 3]> {
        if ws.floating().is_empty() {
            let ranges = self.desktop_window_groups(ws, geo)?;
            let titles = [
                "Offscreen to the left",
                "On desktop",
                "Offscreen to the right",
            ];
            let widths = [172., 114., 180.];
            let mut ranges = ranges.into_iter();
            return Some(std::array::from_fn(|index| {
                let range = ranges.next().unwrap();
                (!range.is_empty()).then_some(OverviewGroup {
                    range,
                    title: titles[index],
                    minimum_width: widths[index],
                    style: if index == 1 {
                        GroupStyle::Desktop
                    } else {
                        GroupStyle::Offscreen
                    },
                })
            }));
        }

        let positions = self.floating_overview_positions(ws, progress);
        if positions.is_empty() {
            return None;
        }
        let zoom = self.overview_zoom();
        let area = self.overview_content();
        let project = |range: Range<f64>| {
            let x = |x: f64| (geo.loc.x + zoom * x).clamp(area.loc.x, area.loc.x + area.size.w);
            x(range.start)..x(range.end)
        };
        let tiled = visible_extent(ws.scrolling().tiles_with_render_positions()).map(|extent| {
            let view = ws.working_area();
            let tolerance = 1. / self.scale.fractional_scale();
            let on_desktop = extent.start >= view.loc.x - tolerance
                && extent.end <= view.loc.x + view.size.w + tolerance;
            OverviewGroup {
                range: project(extent),
                title: if on_desktop {
                    "On desktop · Tiled"
                } else {
                    "Some offscreen · Tiled"
                },
                minimum_width: 168.,
                style: if on_desktop {
                    GroupStyle::Desktop
                } else {
                    GroupStyle::Offscreen
                },
            }
        });
        let floating = visible_extent(
            ws.tiles_with_overview_positions(&positions)
                .filter(|(tile, _, _)| ws.is_floating(tile.window().id())),
        )?;
        let (title, minimum_width, style) = if ws.is_floating_visible() {
            ("On desktop · Floating", 172., GroupStyle::Desktop)
        } else {
            ("Hidden on desktop · Floating", 228., GroupStyle::Hidden)
        };
        Some([
            tiled,
            Some(OverviewGroup {
                range: project(floating),
                title,
                minimum_width,
                style,
            }),
            None,
        ])
    }

    fn render_overview_captions<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
        pointer: Option<Point<f64, Logical>>,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let Some(progress) = self.overview_progress_value() else {
            return;
        };
        let alpha = ((progress - 0.35) / 0.65).clamp(0., 1.) as f32;
        if alpha == 0. {
            return;
        }
        let label_alpha = ((progress - 0.82) / 0.18).clamp(0., 1.) as f32;
        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();
        let area = self.overview_content();
        let left = area.loc.x;
        let right = area.loc.x + area.size.w;
        let contains_y = |y, height| y >= area.loc.y && y + height <= area.loc.y + area.size.h;
        let mut draw = |element| {
            let element = RescaleRenderElement::from_element(element, Point::default(), 1.);
            push(RelocateRenderElement::from_element(
                element,
                Point::default(),
                Relocate::Relative,
            ));
        };

        for ((index, ws), geo) in self.workspaces_with_render_geo_idx() {
            let active = index == self.active_workspace_idx;
            let accent = ws.options.layout.border.active_color;
            let backdrop = ws.options.layout.background_color;
            let empty = !ws.has_windows_or_name();
            let top = if empty && !active {
                geo.loc.y + 11.
            } else {
                geo.loc.y - OVERVIEW_HEADER_OFFSET
            };
            let x = left + OVERVIEW_INSET;
            let width = (right - x - OVERVIEW_INSET).min(235.);
            if contains_y(top, 28.) && width >= 36. {
                if let Some(caption) = ws.overview_caption.render(
                    renderer.as_gles_renderer(),
                    index + 1,
                    ws.name().map(String::as_str),
                    empty,
                    caption_background(accent, backdrop, active),
                    width,
                    scale,
                    Point::from((x, top)),
                    alpha,
                ) {
                    draw(MonitorInnerRenderElement::Caption(caption));
                }
            }

            let command = active.then(|| self.overview_command()).flatten();
            if let Some((rect, window)) = command {
                let hovered = pointer.is_some_and(|point| rect.contains(point));
                let background = if hovered {
                    tint_caption(accent, caption_ink(accent))
                } else {
                    accent
                };
                let action = if ws
                    .active_window()
                    .is_some_and(|active| active.id() == window.id())
                {
                    "Return to window".to_owned()
                } else {
                    let title = navigation_title(window, target, "window");
                    let suffix = window_title_suffix(&title);
                    let distinct = ws
                        .windows()
                        .filter(|other| other.id() != window.id())
                        .map(|other| navigation_title(other, target, "window"))
                        .all(|other| window_title_suffix(&other) != suffix);
                    format!("Switch to {}", compact_window_title(&title, distinct))
                };
                if let Some(command) = self.overview_command_label.render(
                    renderer.as_gles_renderer(),
                    action,
                    background,
                    LabelKind::OverviewCommand,
                    rect.size.w,
                    scale,
                    rect.loc,
                    label_alpha,
                ) {
                    draw(MonitorInnerRenderElement::Caption(command));
                }
            }
            if active && ws.active_window().is_some() && contains_y(top, OVERVIEW_CONTROL_HEIGHT) {
                let surface = caption_background(accent, backdrop, false);
                let mixed_layers = !ws.scrolling().is_empty() && !ws.floating().is_empty();
                let tab = mixed_layers.then(|| {
                    let selected_is_floating = self
                        .overview_selected_window()
                        .is_some_and(|window| ws.is_floating(window.id()));
                    if selected_is_floating {
                        "Select a tiled window"
                    } else {
                        "Select a floating window"
                    }
                });
                let width = if mixed_layers { 350. } else { 154. };
                let header = Point::from((right - OVERVIEW_INSET - width, top));
                let position = command
                    .map(|(rect, _)| Point::from((rect.loc.x - 12. - width, rect.loc.y)))
                    .filter(|point| point.x >= x)
                    .or_else(|| (header.x >= x + 235.).then_some(header));
                if let Some(position) = position {
                    if let Some(label) = self.overview_shortcuts_label.render(
                        renderer.as_gles_renderer(),
                        "Select window".into(),
                        surface,
                        LabelKind::NavigationHint { tab },
                        width,
                        scale,
                        position,
                        label_alpha,
                    ) {
                        draw(MonitorInnerRenderElement::Caption(label));
                    }
                }
            }

            let mut hidden_group_labeled = false;
            if let Some(groups) = active
                .then(|| self.overview_groups(ws, geo, progress))
                .flatten()
            {
                let y = geo.loc.y + self.overview_strip_offset(ws) * zoom - 36.;
                if contains_y(y, 28.) {
                    let surface = caption_background(accent, backdrop, false);
                    for (group, label) in
                        groups.into_iter().zip(&ws.overview_caption.desktop_labels)
                    {
                        let Some(group) = group else { continue };
                        let width = group.range.end - group.range.start;
                        if width < group.minimum_width {
                            continue;
                        }
                        if let Some(caption) = label.render(
                            renderer.as_gles_renderer(),
                            group.title.into(),
                            surface,
                            LabelKind::GroupTitle,
                            group.minimum_width,
                            scale,
                            Point::from((group.range.start, y)),
                            label_alpha,
                        ) {
                            hidden_group_labeled |= group.style == GroupStyle::Hidden;
                            draw(MonitorInnerRenderElement::Caption(caption));
                        }
                    }
                }
            }

            if label_alpha == 0. {
                continue;
            }
            let mut in_front: Vec<Rectangle<f64, Logical>> = Vec::new();
            let focused = active.then(|| self.overview_selected_window()).flatten();
            let preview_positions = self.floating_overview_positions(ws, progress);
            for (tile, pos, visible) in ws.tiles_with_overview_positions(&preview_positions) {
                if !visible {
                    continue;
                }
                let location = geo.loc + pos.upscale(zoom);
                let size = tile.tile_size().upscale(zoom);
                if location.x + size.w < left || location.x > right {
                    continue;
                }
                let rect = Rectangle::new(location, size);
                let label = Rectangle::new(
                    location + Point::from((1., 1.)),
                    Size::from((size.w - 2., 22.)),
                );
                let covered = in_front
                    .iter()
                    .any(|front| front.intersection(label).is_some());
                in_front.push(rect);
                let window = tile.window();
                if covered
                    || size.h < 40.
                    || label.size.w < 65.
                    || !contains_y(label.loc.y, label.size.h)
                    || location.x < left
                    || location.x + size.w > right
                    || target.should_block_out(window.rules().block_out_from)
                {
                    continue;
                }

                let selected = focused.is_some_and(|focused| focused.id() == window.id());
                let hidden = !hidden_group_labeled
                    && !ws.is_floating_visible()
                    && ws.is_floating(window.id());
                let desktop_focus = active
                    && ws
                        .active_window()
                        .is_some_and(|active_window| active_window.id() == window.id());
                let current = desktop_focus && !selected;
                let surface = caption_background(accent, backdrop, false);
                let selection_color = if desktop_focus {
                    accent
                } else {
                    caption_ink(surface)
                };
                let badge = if hidden {
                    Some("Hidden")
                } else if current {
                    Some(if label.size.w < 220. {
                        "Current"
                    } else {
                        "Current focus"
                    })
                } else if selected && !desktop_focus && label.size.w >= 164. {
                    Some("Selected")
                } else {
                    None
                };
                if selected && contains_y(rect.loc.y, rect.size.h) {
                    ws.overview_caption.render_selection(
                        renderer,
                        rect,
                        selection_color,
                        label_alpha,
                        scale,
                        &mut |outline| draw(MonitorInnerRenderElement::Decoration(outline)),
                    );
                }
                window.with_overview_title(|title| {
                    let title: String = title
                        .chars()
                        .take(512)
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect();
                    if title.trim().is_empty() {
                        return;
                    }
                    let background = if current {
                        tint_caption(surface, accent)
                    } else if desktop_focus {
                        accent
                    } else if selected {
                        tint_caption(surface, selection_color)
                    } else {
                        surface
                    };
                    if let Some(label) = tile.overview_label.render(
                        renderer.as_gles_renderer(),
                        title,
                        background,
                        LabelKind::Preview { badge, current },
                        label.size.w,
                        scale,
                        label.loc,
                        label_alpha,
                    ) {
                        draw(MonitorInnerRenderElement::Caption(label));
                    }
                });
            }
        }
    }

    pub fn render_workspaces<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        focus_ring: bool,
        pointer: Option<Point<f64, Logical>>,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let _span = tracy_client::span!("Monitor::render_workspaces");

        let scale = self.scale.fractional_scale();
        // Ceil the height in physical pixels.
        let height = (self.view_size.h * scale).ceil() as i32;

        let zoom = self.overview_zoom();

        if self.options.overview.framed_workspaces {
            self.render_overview_captions(ctx.renderer, ctx.target, pointer, push);
        }
        self.render_scroll_cues(ctx.renderer, ctx.target, pointer, push);

        let insert_hint_render_loc = self
            .insert_hint_render_loc
            .filter(|_| !self.options.layout.insert_hint.off);

        let scale_relocate = move |geo: Rectangle<f64, Logical>, elem| {
            let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
            RelocateRenderElement::from_element(
                elem,
                // The offset we get from workspaces_with_render_geo() is already
                // rounded to physical pixels, but it's in the logical coordinate
                // space, so we need to convert it to physical.
                geo.loc.to_physical_precise_round(scale),
                Relocate::Relative,
            )
        };

        // Draw in passes for correct Z ordering during window movement between workspaces:
        // - floating windows moving between workspaces
        // - normal floating windows
        // - scrolling windows moving between workspaces
        // - normal scrolling windows
        for pass in 0..4 {
            // Don't cull when drawing windows moving between workspaces so that windows moving to
            // workspaces off-screen will still render.
            let cull = matches!(pass, 1 | 3);

            // Crop the elements to prevent them overflowing, currently visible during a workspace
            // switch.
            //
            // HACK: crop to infinite bounds at least horizontally where we
            // know there's no workspace joining or monitor bounds, otherwise
            // it will cut pixel shaders and mess up the coordinate space.
            // There's also a damage tracking bug which causes glitched
            // rendering for maximized GTK windows.
            //
            // FIXME: use proper bounds after fixing the Crop element.
            //
            // Also, check cull here to avoid cropping windows moving between workspaces.
            //
            // FIXME: for cull=true, it might be better visually to crop to a workspace-high region
            // anchored to the window/column as it moves between workspaces, to prevent overflowing
            // windows from appearing and disappearing.
            let crop_bounds =
                if cull && (self.workspace_switch.is_some() || self.overview_progress.is_some()) {
                    Rectangle::new(
                        Point::from((-i32::MAX / 2, 0)),
                        Size::from((i32::MAX, height)),
                    )
                } else {
                    Rectangle::new(
                        Point::from((-i32::MAX / 2, -i32::MAX / 2)),
                        Size::from((i32::MAX, i32::MAX)),
                    )
                };

            for (ws, geo) in self.workspaces_with_render_geo_cull(cull) {
                let mut crop_bounds = if cull
                    && self.options.overview.framed_workspaces
                    && self.overview_progress.is_some()
                {
                    self.overview_crop(geo, scale, zoom, height)
                } else {
                    crop_bounds
                };
                let visible_x = (pass == 3)
                    .then(|| self.scrolling_visible_span(ws))
                    .flatten();
                let mut shadow_crop = crop_bounds;
                if let Some(bounds) = &visible_x {
                    let left = ((bounds.start - geo.loc.x) * scale).ceil() as i32;
                    let right = ((bounds.end - geo.loc.x) * scale).floor() as i32;
                    crop_bounds.loc.x = left;
                    crop_bounds.size.w = right.saturating_sub(left).max(0);
                    if let Some(top) = self.navigation_strip_top() {
                        let boundary = ((top + NAVIGATION_STRIP_HEIGHT) * scale).ceil() as i32;
                        for crop in [&mut crop_bounds, &mut shadow_crop] {
                            let bottom = crop.loc.y.saturating_add(crop.size.h);
                            crop.loc.y = crop.loc.y.max(boundary);
                            crop.size.h = bottom.saturating_sub(crop.loc.y).max(0);
                        }
                    }
                    // A visible tile's soft shadow can extend into the gap
                    // next to its window. Clip it only at the panel bounds.
                    if let Some(panels) = self.scroll_clip(ws) {
                        let left = ((panels.start - geo.loc.x) * scale).ceil() as i32;
                        let right = ((panels.end - geo.loc.x) * scale).floor() as i32;
                        shadow_crop.loc.x = left;
                        shadow_crop.size.w = right.saturating_sub(left).max(0);
                    }
                }
                // Macro instead of closure because ws and insert hint have different elem types.
                macro_rules! push {
                    () => {{
                        &mut |elem| {
                            let elem = CropRenderElement::from_element(elem, scale, crop_bounds);
                            if let Some(elem) = elem {
                                let elem = MonitorInnerRenderElement::from(elem);
                                push(scale_relocate(geo, elem));
                            }
                        }
                    }};
                }

                let xray_pos = XrayPos::new(geo.loc, zoom);

                let floating_positions = if pass < 2 {
                    self.overview_progress_value()
                        .map(|progress| self.floating_overview_positions(ws, progress))
                } else {
                    None
                };
                match pass {
                    0 => {
                        ws.render_floating(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::MovingBetweenWorkspaces,
                            floating_positions.as_deref(),
                            push!(),
                        );
                    }
                    1 => {
                        ws.render_floating(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::Normal,
                            floating_positions.as_deref(),
                            push!(),
                        );

                        if let Some(loc) = insert_hint_render_loc {
                            if loc.workspace == InsertWorkspace::Existing(ws.id()) {
                                self.insert_hint_element.render(
                                    ctx.renderer,
                                    loc.location,
                                    push!(),
                                );
                            }
                        }
                    }
                    2 => {
                        ws.render_scrolling(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::MovingBetweenWorkspaces,
                            None,
                            push!(),
                        );
                    }
                    _ => {
                        ws.render_scrolling(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::Normal,
                            visible_x.as_ref(),
                            &mut |elem| {
                                let clip = if elem.is_window_shadow() {
                                    shadow_crop
                                } else {
                                    crop_bounds
                                };
                                if let Some(elem) =
                                    CropRenderElement::from_element(elem, scale, clip)
                                {
                                    push(scale_relocate(geo, elem.into()));
                                }
                            },
                        );
                    }
                }
            }
        }
    }

    pub fn has_navigation_controls(&self) -> bool {
        self.has_framed_overview()
            || self
                .active_workspace_ref()
                .options
                .layout
                .clip_scroll_to_work_area
    }

    pub fn has_framed_overview(&self) -> bool {
        self.options.overview.framed_workspaces && self.overview_progress.is_some()
    }

    fn overview_frame_geometry(
        &self,
        ws: &Workspace<W>,
        index: usize,
        geo: Rectangle<f64, Logical>,
    ) -> Option<Rectangle<f64, Logical>> {
        if !self.has_framed_overview() {
            return None;
        }
        let progress = self.overview_progress_value()?.clamp(0., 1.);
        let margin = OVERVIEW_FRAME_MARGIN.min(self.working_area.size.w / 2.);
        let empty = index != self.active_workspace_idx && !ws.has_windows_or_name();
        let area = self.overview_content();
        let (left, width) = if empty {
            (area.loc.x + OVERVIEW_INSET, area.size.w.min(188.))
        } else {
            (
                self.working_area.loc.x + margin,
                self.working_area.size.w - 2. * margin,
            )
        };
        let height = if empty {
            geo.size.h.min(28.)
        } else {
            geo.size.h + self.overview_strip_offset(ws) * self.overview_zoom()
        };
        let y = if empty {
            geo.loc.y + 11. * progress
        } else {
            geo.loc.y
        };
        let position = Point::from((geo.loc.x + (left - geo.loc.x) * progress, y));
        let size = Size::from((
            geo.size.w + (width - geo.size.w) * progress,
            geo.size.h + (height - geo.size.h) * progress,
        ));
        let scale = self.scale.fractional_scale();
        Rectangle::new(position, size)
            .intersection(area)
            .map(|rect| rect.to_physical_precise_round(scale).to_logical(scale))
    }

    pub fn render_overview_frames<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let progress = self
            .overview_progress_value()
            .unwrap_or_default()
            .clamp(0., 1.);
        let scale = self.scale.fractional_scale();
        let shadow_config = compute_workspace_shadow_config(
            self.options.overview.workspace_shadow,
            self.workspace_size(self.overview_zoom()),
        );
        for ((index, ws), geo) in self.workspaces_with_render_geo_idx() {
            let Some(rect) = self.overview_frame_geometry(ws, index, geo) else {
                continue;
            };
            let active = index == self.active_workspace_idx;
            let group_alpha = ((progress - 0.80) / 0.20).clamp(0., 1.) as f32;
            let groups = active
                .then(|| self.overview_groups(ws, geo, progress))
                .flatten();
            let strip_offset = self.overview_strip_offset(ws) * self.overview_zoom();
            let mut draw = |elem| {
                let elem = MonitorInnerRenderElement::Decoration(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                push(RelocateRenderElement::from_element(
                    elem,
                    Point::default(),
                    Relocate::Relative,
                ));
            };
            let groups = groups.filter(|_| group_alpha > 0.).map(|groups| {
                std::array::from_fn::<_, 3, _>(|index| {
                    let group = groups[index].as_ref()?;
                    let range = &group.range;
                    if range.is_empty() {
                        return None;
                    }
                    Rectangle::new(
                        Point::from((range.start, geo.loc.y + strip_offset - 7.)),
                        Size::from((range.end - range.start, 2.)),
                    )
                    .intersection(self.overview_content())
                    .map(|rect| {
                        (
                            rect.to_physical_precise_round(scale).to_logical(scale),
                            group.style,
                        )
                    })
                })
            });

            if let Some(groups) = &groups {
                let zoom = self.overview_zoom();
                let positions = self.floating_overview_positions(ws, progress);
                for (index, group) in groups.iter().enumerate() {
                    let Some((group, style)) = group else {
                        continue;
                    };
                    ws.overview_caption.render_group_rule(
                        index,
                        *group,
                        *style,
                        ws.options.layout.background_color,
                        ws.options.layout.border.active_color,
                        group_alpha,
                        &mut draw,
                    );
                    let top = geo.loc.y + strip_offset - 39.;
                    let bottom = ws
                        .tiles_with_overview_positions(&positions)
                        .filter_map(|(tile, pos, visible)| {
                            let left = geo.loc.x + pos.x * zoom;
                            let right = left + tile.tile_size().w * zoom;
                            let inside = left < group.loc.x + group.size.w && right > group.loc.x;
                            (visible && inside)
                                .then_some(geo.loc.y + (pos.y + tile.tile_size().h) * zoom)
                        })
                        .reduce(f64::max)
                        .unwrap_or(geo.loc.y + geo.size.h)
                        + 6.;
                    let backing = Rectangle::new(
                        Point::from((group.loc.x, top)),
                        Size::from((group.size.w, (bottom - top).max(0.))),
                    );
                    if let Some(backing) = backing.intersection(self.overview_content()) {
                        ws.overview_caption.render_group_backing(
                            renderer,
                            index,
                            backing,
                            *style,
                            ws.options.layout.background_color,
                            group_alpha,
                            scale,
                            &mut draw,
                        );
                    }
                }
            }
            let fade = if groups.is_some() || !active && !ws.has_windows_or_name() {
                group_alpha
            } else {
                0.
            };
            let frame_alpha = progress as f32 * (1. - fade);
            if frame_alpha > 0. {
                ws.overview_caption.render_frame(
                    renderer,
                    rect,
                    ws.options.layout.background_color,
                    active,
                    frame_alpha,
                    scale,
                    &mut draw,
                );
            }
            if !shadow_config.on || frame_alpha == 0. {
                continue;
            }
            let mut shadow = ws.overview_caption.shadow.borrow_mut();
            shadow.update_config(shadow_config);
            shadow.update_render_elements(
                rect.size,
                active,
                frame_radius(rect, progress as f32),
                scale,
                frame_alpha,
            );
            shadow.render(renderer, rect.loc, &mut |elem| {
                let elem = MonitorInnerRenderElement::Shadow(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                push(RelocateRenderElement::from_element(
                    elem,
                    Point::default(),
                    Relocate::Relative,
                ));
            });
        }
    }

    pub fn render_workspace_shadows<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let Some(progress) = self.overview_progress.as_ref().map(|p| p.clamped_value()) else {
            return;
        };
        let alpha = progress.clamp(0., 1.) as f32;

        let _span = tracy_client::span!("Monitor::render_workspace_shadows");

        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();

        for (ws, geo) in self.workspaces_with_render_geo() {
            ws.render_shadow(renderer, &mut |elem| {
                let elem = elem.with_alpha(alpha);
                let elem = MonitorInnerRenderElement::Shadow(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
                let elem = RelocateRenderElement::from_element(
                    elem,
                    geo.loc.to_physical_precise_round(scale),
                    Relocate::Relative,
                );
                push(elem);
            });
        }
    }

    pub fn workspace_switch_gesture_begin(&mut self, is_touchpad: bool) {
        let center_idx = self.active_workspace_idx;
        let current_idx = self.workspace_render_idx();

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad,
            is_clamped: !self.overview_open,
            dnd_last_event_time: None,
            dnd_nonzero_start_time: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        if let Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
            dnd_last_event_time: Some(_),
            ..
        })) = &self.workspace_switch
        {
            // Already active.
            return;
        }

        if !self.overview_open {
            // This gesture is only for the overview.
            return;
        }

        let center_idx = self.active_workspace_idx;
        let current_idx = self.workspace_render_idx();

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad: false,
            is_clamped: false,
            dnd_last_event_time: Some(self.clock.now_unadjusted()),
            dnd_nonzero_start_time: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn workspace_switch_gesture_update(
        &mut self,
        delta_y: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return None;
        };

        if gesture.is_touchpad != is_touchpad || gesture.dnd_last_event_time.is_some() {
            return None;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return None;
        };

        // Reduce the effect of zoom on the touchpad somewhat.
        let delta_scale = if gesture.is_touchpad {
            (zoom - 1.) / 2.5 + 1.
        } else {
            zoom
        };

        let delta_y = delta_y / delta_scale;
        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        gesture.tracker.push(delta_y, timestamp);

        let pos = gesture.tracker.pos() / total_height;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let new_idx = gesture.start_idx + pos;
        let new_idx = rubber_band.clamp(min, max, new_idx);

        if gesture.current_idx == new_idx {
            return Some(false);
        }

        gesture.current_idx = new_idx;
        Some(true)
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, pos: Point<f64, Logical>, speed: f64) -> bool {
        let zoom = self.overview_zoom();

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        let Some(last_time) = gesture.dnd_last_event_time else {
            // Not a DnD scroll.
            return false;
        };

        let config = &self.options.gestures.dnd_edge_workspace_switch;
        let trigger_height = config.trigger_height;

        // Restrict the scrolling horizontally to the strip of workspaces to avoid unwanted trigger
        // after using the hot corner or during horizontal scroll.
        let width = self.view_size.w * zoom;
        let x = pos.x - (self.view_size.w - width) / 2.;

        // Consider the working area so layer-shell docks and such don't prevent scrolling.
        let y = pos.y - self.working_area.loc.y;
        let height = self.working_area.size.h;

        let y = y.clamp(0., height);
        let trigger_height = trigger_height.clamp(0., height / 2.);

        let delta = if x < 0. || width <= x {
            // Outside the bounds horizontally.
            0.
        } else if y < trigger_height {
            -(trigger_height - y)
        } else if height - y < trigger_height {
            trigger_height - (height - y)
        } else {
            0.
        };

        let delta = if trigger_height < 0.01 {
            // Sanity check for trigger-height 0 or small window sizes.
            0.
        } else {
            // Normalize to [0, 1].
            delta / trigger_height
        };
        let delta = delta * speed;

        let now = self.clock.now_unadjusted();
        gesture.dnd_last_event_time = Some(now);

        if delta == 0. {
            // We're outside the scrolling zone.
            gesture.dnd_nonzero_start_time = None;
            return false;
        }

        let nonzero_start = *gesture.dnd_nonzero_start_time.get_or_insert(now);

        // Delay starting the gesture a bit to avoid unwanted movement when dragging across
        // monitors.
        let delay = Duration::from_millis(u64::from(config.delay_ms));
        if now.saturating_sub(nonzero_start) < delay {
            return true;
        }

        let time_delta = now.saturating_sub(last_time).as_secs_f64();

        let delta = delta * time_delta * config.max_speed;

        gesture.tracker.push(delta, now);

        let total_height = WORKSPACE_DND_EDGE_SCROLL_MOVEMENT;
        let pos = gesture.tracker.pos() / total_height;
        let unclamped = gesture.start_idx + pos;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let clamped = unclamped.clamp(min, max);

        // Make sure that DnD scrolling too much outside the min/max does not "build up".
        gesture.start_idx += clamped - unclamped;
        gesture.current_idx = clamped;

        true
    }

    pub fn workspace_switch_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return false;
        };

        if is_touchpad.is_some_and(|x| gesture.is_touchpad != x) {
            return false;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.dnd_last_event_time.is_some() {
            WORKSPACE_DND_EDGE_SCROLL_MOVEMENT
        } else if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        let mut velocity = gesture.tracker.velocity() / total_height;
        let current_pos = gesture.tracker.pos() / total_height;
        let pos = gesture.tracker.projected_end_pos() / total_height;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let new_idx = gesture.start_idx + pos;

        let new_idx = new_idx.clamp(min, max);
        let new_idx = new_idx.round() as usize;

        velocity *= rubber_band.clamp_derivative(min, max, gesture.start_idx + current_pos);

        if self.active_workspace_idx != new_idx {
            self.previous_workspace_id = Some(self.workspaces[self.active_workspace_idx].id());
        }

        self.active_workspace_idx = new_idx;
        self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
            self.clock.clone(),
            gesture.current_idx,
            new_idx as f64,
            velocity,
            self.options.animations.workspace_switch.0,
        )));

        true
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        if !matches!(
            self.workspace_switch,
            Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
                dnd_last_event_time: Some(_),
                ..
            }))
        ) {
            // Not a DnD scroll.
            return;
        };

        self.workspace_switch_gesture_end(None);
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&niri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub(super) fn verify_invariants(&self) {
        use approx::assert_abs_diff_eq;

        let options =
            Options::clone(&self.base_options).with_merged_layout(self.layout_config.as_ref());
        assert_eq!(&*self.options, &options);

        assert!(
            !self.workspaces.is_empty(),
            "monitor must have at least one workspace"
        );
        assert!(self.active_workspace_idx < self.workspaces.len());

        if let Some(WorkspaceSwitch::Animation(anim)) = &self.workspace_switch {
            let before_idx = anim.from() as usize;
            let after_idx = anim.to() as usize;

            assert!(before_idx < self.workspaces.len());
            assert!(after_idx < self.workspaces.len());
        }

        assert!(
            !self.workspaces.last().unwrap().has_windows(),
            "monitor must have an empty workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                !self.workspaces.first().unwrap().has_windows(),
                "first workspace must be empty when empty_workspace_above_first is set"
            )
        }

        assert!(
            self.workspaces.last().unwrap().name.is_none(),
            "monitor must have an unnamed workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.workspaces.first().unwrap().name.is_none(),
                "first workspace must be unnamed when empty_workspace_above_first is set"
            )
        }

        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.workspaces.len() != 2,
                "if empty_workspace_above_first is set there must be just 1 or 3+ workspaces"
            )
        }

        // If there's no workspace switch in progress, there can't be any non-last non-active
        // empty workspaces. If empty_workspace_above_first is set then the first workspace
        // will be empty too.
        let pre_skip = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        if self.workspace_switch.is_none() {
            for (idx, ws) in self
                .workspaces
                .iter()
                .enumerate()
                .skip(pre_skip)
                .rev()
                // skip last
                .skip(1)
            {
                if idx != self.active_workspace_idx {
                    assert!(
                        ws.has_windows_or_name(),
                        "non-active workspace can't be empty and unnamed except the last one"
                    );
                }
            }
        }

        for workspace in &self.workspaces {
            assert_eq!(self.clock, workspace.clock);

            assert_eq!(
                self.scale().integer_scale(),
                workspace.scale().integer_scale()
            );
            assert_eq!(
                self.scale().fractional_scale(),
                workspace.scale().fractional_scale()
            );
            assert_eq!(self.view_size, workspace.view_size());
            assert_eq!(self.working_area, workspace.working_area());

            assert_eq!(
                workspace.base_options, self.options,
                "workspace options must be synchronized with monitor"
            );
        }

        let scale = self.scale().fractional_scale();
        let iter = self.workspaces_with_render_geo();
        for (_ws, ws_geo) in iter {
            let pos = ws_geo.loc;
            let rounded_pos = pos.to_physical_precise_round(scale).to_logical(scale);

            // Workspace positions must be rounded to physical pixels.
            assert_abs_diff_eq!(pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(pos.y, rounded_pos.y, epsilon = 1e-5);
        }
    }
}

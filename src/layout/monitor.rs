use std::cell::{Cell, RefCell};
use std::cmp::min;
use std::collections::HashMap;
use std::iter::zip;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use niri_config::layout::WindowBar;
use niri_config::{Color, CornerRadius, LayoutPart};
use niri_ipc::PinSide;
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::insert_hint_element::{InsertHintElement, InsertHintRenderElement};
use super::layout_menu::{self, LayoutMenuItem, LayoutMenuState, LayoutMenuTexture};
use super::overview_caption::{
    caption_background, caption_ink, is_light, measure_bar_label, set_caption_font, tint_caption,
    BarIcon, BarPillState, LabelKind, PreviewLabel, BAR_CARD_PADDING, BAR_CARD_TITLE_HEIGHT,
    BAR_PILL_HEIGHT, BAR_SHADOW_MARGIN, BAR_TRAY_HEIGHT, HEIGHT as WORKSPACE_LABEL_HEIGHT,
    PADDING as WORKSPACE_LABEL_INSET, SCROLL_BUTTON_HEIGHT, SCROLL_CARD_HEIGHT,
};
use super::scrolling::{navigation_strip_fits, Column, ColumnWidth, NAVIGATION_STRIP_HEIGHT};
use super::tile::Tile;
use super::titlebar::TitlebarPart;
use super::workspace::{
    compute_working_area, OutputId, Workspace, WorkspaceAddWindowTarget, WorkspaceId,
    WorkspaceRenderElement,
};
use super::SnapLines;
use super::{
    compute_overview_zoom, ActivateWindow, HitType, LayoutElement, LayoutElementRenderElement,
    Options,
};
use crate::animation::{Animation, Clock};
use crate::input::swipe_tracker::SwipeTracker;
use crate::layout::RenderLayer;
use crate::niri_render_elements;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::rubber_band::RubberBand;
use crate::utils::transaction::Transaction;
use crate::utils::{
    output_size, round_logical_in_physical, round_logical_in_physical_max1, ResizeEdge,
};

/// Amount of touchpad movement to scroll the height of one workspace.
const WORKSPACE_GESTURE_MOVEMENT: f64 = 300.;

const MAX_NAVIGATION_TITLES: usize = 8;

/// Corner radius of a workspace card in the overview, on screen.
const WORKSPACE_CARD_RADIUS: f64 = 16.;
/// How long a window bar shown on demand stays after the last keyboard move between windows.
const NAVIGATION_BAR_LINGER: Duration = Duration::from_millis(1200);
/// How close to the edge of the scrolling area the pointer brings out a scroll arrow, which
/// is fully shown within `CUE_FULL_REACH`.
const CUE_REACH: f64 = 140.;
const CUE_FULL_REACH: f64 = 64.;
/// Space between neighboring controls in the window bar.
const BAR_PILL_GAP: f64 = 4.;
const BAR_PILL_MIN_WIDTH: f64 = 96.;
const BAR_PILL_MAX_WIDTH: f64 = 220.;
/// Share of the work area the window bar may take before its names shrink.
const BAR_MAX_SHARE: f64 = 0.86;
/// Space on either side of the line between tiled and floating windows.
const BAR_DIVIDER_GAP: f64 = 8.;
/// Space between the bar of tiled windows and the bar of floating ones.
const BAR_GROUP_GAP: f64 = 8.;
/// Narrowest width of the bar of tiled windows.
const BAR_MIN_WIDTH: f64 = 600.;
/// Minimum space between the window bar and the sides of the work area.
const BAR_MARGIN: f64 = 24.;

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
    /// The focused style of each name, shown inside the focus fill while it slides.
    strip_focus_labels: [PreviewLabel; MAX_NAVIGATION_TITLES],
    strip_card: PreviewLabel,
    strip_floating_tray: PreviewLabel,
    /// The window whose entry in the window bar shows focus.
    bar_focus: RefCell<Option<W::Id>>,
    /// The focus fill of the window bar, sliding from one name to the next.
    bar_highlight: RefCell<Option<BarHighlight<W::Id>>>,
    bar_highlight_fill: RefCell<BorderRenderElement>,
    bar_highlight_fade: RefCell<BorderRenderElement>,
    /// Dimming over columns that are mostly out of view.
    peek_scrims: RefCell<Vec<BorderRenderElement>>,
    peek_scrim_labels: RefCell<Vec<PreviewLabel>>,
    arrow_tooltips: [PreviewLabel; 2],
    strip_edge_labels: [PreviewLabel; 2],
    dismissed_preview_arrow: Cell<Option<Rectangle<f64, Logical>>>,
    /// The pointer on this output, which brings the scroll arrows out near the edges.
    cue_pointer: Cell<Option<Point<f64, Logical>>>,
    /// Scroll arrows, left and right, that the pointer reached through the gap at their edge
    /// and that are open as full, clickable pills.
    /// The height at which each opened, so it stays put under the pointer.
    cue_armed: Cell<[Option<f64>; 2]>,
    /// How far the window bar shown on demand is revealed.
    bar_reveal: RefCell<BarReveal>,
    /// The layout menu open under a titlebar's layout button.
    layout_menu: RefCell<Option<OpenLayoutMenu<W::Id>>>,
    layout_menu_texture: LayoutMenuTexture,
    layer_action_label: PreviewLabel,
    scratchpad_label: PreviewLabel,
    /// Names above the zoomed-out workspaces with `overview { workspace-cards; }`.
    workspace_labels: RefCell<Vec<PreviewLabel>>,
    /// How many windows the scratchpad holds, on every output.
    scratchpad_count: Cell<usize>,
    scroll_cue_labels: [PreviewLabel; 2],
    /// Tabs of folded pinned columns, left and right, and their name pills.
    folded_tab_labels: [PreviewLabel; 2],
    folded_name_labels: [PreviewLabel; 2],
    /// The folded tab the pointer is on, which shows the column's name.
    folded_hover: Cell<Option<PinSide>>,
    additional_scroll_cue_labels: [PreviewLabel; 2],
    scroll_edge_fades: [PreviewLabel; 2],
    /// Location to render the insert hint element.
    insert_hint_render_loc: Option<InsertHintRenderLoc>,
    /// Whether the overview is open.
    pub(super) overview_open: bool,
    /// Progress of the overview zoom animation, 1 is fully in overview.
    overview_progress: Option<OverviewProgress>,
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

struct NavigationStrip<'a, W> {
    tray: Option<Rectangle<f64, Logical>>,
    moving: bool,
    edge_actions: [Option<NavigationArrow<'a, W>>; 2],
    titles: Vec<NavigationTitle<'a, W>>,
    layer_action: Option<(Rectangle<f64, Logical>, &'a W)>,
    /// The scratchpad's button, while it holds windows.
    scratchpad_action: Option<Rectangle<f64, Logical>>,
    /// The separate bar of floating windows.
    floating_tray: Option<Rectangle<f64, Logical>>,
}

#[derive(Debug)]
struct BarHighlight<Id> {
    window: Id,
    previous: Option<Id>,
    from: Rectangle<f64, Logical>,
    to: Rectangle<f64, Logical>,
    anim: Animation,
}

impl<Id> BarHighlight<Id> {
    /// Whether other names lie between the two ends: the fill then crossfades rather than
    /// sliding under them. To a neighbour it slides, which shows the direction.
    fn leaps(&self) -> bool {
        let gap = 12.;
        self.to.loc.x > self.from.loc.x + self.from.size.w + gap
            || self.to.loc.x + self.to.size.w + gap < self.from.loc.x
    }

    /// The sliding fill, while it is between two names.
    fn sliding(&self) -> Option<Rectangle<f64, Logical>> {
        (!self.leaps() && !self.anim.is_done()).then(|| self.rect())
    }

    /// The fills to draw with their opacity: one easing in place, or two crossfading. The old
    /// one is gone halfway through, so no ghost lingers on the name focus left.
    fn fills(&self) -> Vec<(Rectangle<f64, Logical>, f32)> {
        if !self.leaps() || self.anim.is_done() {
            return vec![(self.rect(), 1.)];
        }
        let t = self.anim.clamped_value().clamp(0., 1.) as f32;
        vec![
            (self.from, (1. - 2. * t).max(0.)),
            (self.to, (1.5 * t).min(1.)),
        ]
    }

    /// Where the fill counts as being, for styling the names above it.
    fn anchor(&self) -> Rectangle<f64, Logical> {
        if self.leaps() && !self.anim.is_done() {
            // Where the stronger of the two fills is.
            if self.anim.clamped_value() < 2. / 7. {
                self.from
            } else {
                self.to
            }
        } else {
            self.rect()
        }
    }

    fn rect(&self) -> Rectangle<f64, Logical> {
        let t = self.anim.value();
        let lerp = |a: f64, b: f64| a + (b - a) * t;
        Rectangle::new(
            Point::from((
                lerp(self.from.loc.x, self.to.loc.x),
                lerp(self.from.loc.y, self.to.loc.y),
            )),
            Size::from((
                lerp(self.from.size.w, self.to.size.w),
                lerp(self.from.size.h, self.to.size.h),
            )),
        )
    }
}

struct NavigationArrow<'a, W> {
    rect: Rectangle<f64, Logical>,
    target: Option<&'a W>,
    /// How many windows lie out of view on this side.
    count: usize,
}

struct NavigationTitle<'a, W> {
    rect: Rectangle<f64, Logical>,
    window: &'a W,
    active: bool,
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
    /// Open as a full pill naming the window, rather than a slim handle in the gap.
    armed: bool,
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
        Caption = PrimaryGpuTextureRenderElement,
        ClippedCaption = CropRenderElement<PrimaryGpuTextureRenderElement>,
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

/// The window bar shown on demand: whether it is wanted, and its slide.
#[derive(Debug, Default)]
struct BarReveal {
    shown: bool,
    /// Shown with `toggle-window-bar` rather than by the pointer, so it stays until toggled.
    by_key: bool,
    anim: Option<Animation>,
    /// Shown while keyboard navigation moves between windows: when to hide it again.
    hide_at: Option<Duration>,
}

impl BarReveal {
    fn progress(&self) -> f64 {
        match &self.anim {
            Some(anim) => anim.value().clamp(0., 1.),
            None if self.shown => 1.,
            None => 0.,
        }
    }
}

/// The tab a folded pinned column leaves in the gap at its edge.
struct FoldedTab<'a, W> {
    side: PinSide,
    window: &'a W,
    /// The tab itself, in the gap beside the windows.
    tab: Rectangle<f64, Logical>,
    /// The pill naming the column, shown while the pointer is on the tab.
    name: Rectangle<f64, Logical>,
}

impl<W> FoldedTab<'_, W> {
    /// The tab as drawn: a slim accent handle in the middle of its gap.
    fn drawn_tab(&self) -> Rectangle<f64, Logical> {
        let width = (self.tab.size.w - 4.).clamp(4., 12.);
        Rectangle::new(
            Point::from((
                self.tab.loc.x + (self.tab.size.w - width) / 2.,
                self.tab.loc.y,
            )),
            Size::from((width, self.tab.size.h)),
        )
    }

    /// Where the tab takes the pointer: the whole gap beside it, and its name pill while
    /// that shows.
    fn hit_area(&self, hover: Option<PinSide>) -> Rectangle<f64, Logical> {
        if hover == Some(self.side) {
            self.tab.merge(self.name)
        } else {
            self.tab
        }
    }
}

/// A layout menu opened from a titlebar.
#[derive(Debug)]
struct OpenLayoutMenu<Id> {
    window: Id,
    /// Opened by pointing at the button rather than clicking it; it closes when the pointer
    /// leaves.
    hover: bool,
    hovered: Option<LayoutMenuItem>,
}

/// Where an open layout menu is and what it shows.
struct LayoutMenuGeometry<Id> {
    window: Id,
    anchor: Rectangle<f64, Logical>,
    rect: Rectangle<f64, Logical>,
    state: LayoutMenuState,
    hover: bool,
    hovered: Option<LayoutMenuItem>,
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
        set_caption_font(&options.layout.titlebar.font);

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
            strip_focus_labels: Default::default(),
            strip_card: PreviewLabel::default(),
            strip_floating_tray: PreviewLabel::default(),
            bar_focus: RefCell::new(None),
            bar_highlight: RefCell::new(None),
            bar_highlight_fill: RefCell::new(BorderRenderElement::empty()),
            bar_highlight_fade: RefCell::new(BorderRenderElement::empty()),
            peek_scrims: RefCell::new(Vec::new()),
            peek_scrim_labels: RefCell::new(Vec::new()),
            arrow_tooltips: Default::default(),
            strip_edge_labels: Default::default(),
            dismissed_preview_arrow: Cell::new(None),
            cue_pointer: Cell::new(None),
            cue_armed: Cell::new([None; 2]),
            bar_reveal: RefCell::new(BarReveal::default()),
            layout_menu: RefCell::new(None),
            layout_menu_texture: LayoutMenuTexture::default(),
            layer_action_label: PreviewLabel::default(),
            scratchpad_label: PreviewLabel::default(),
            workspace_labels: RefCell::new(Vec::new()),
            scratchpad_count: Cell::new(0),
            scroll_cue_labels: Default::default(),
            folded_tab_labels: Default::default(),
            folded_name_labels: Default::default(),
            folded_hover: Cell::new(None),
            additional_scroll_cue_labels: Default::default(),
            scroll_edge_fades: Default::default(),
            insert_hint_render_loc: None,
            overview_open: false,
            overview_progress: None,
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

        {
            let mut reveal = self.bar_reveal.borrow_mut();
            if reveal.anim.as_ref().is_some_and(|anim| anim.is_done()) {
                reveal.anim = None;
            }
        }
        let linger_over = self
            .bar_reveal
            .borrow()
            .hide_at
            .is_some_and(|at| self.clock.now_unadjusted() >= at);
        if linger_over {
            self.set_bar_shown(false, false);
        }

        for ws in &mut self.workspaces {
            ws.advance_animations();
        }
    }

    pub(super) fn are_animations_ongoing(&self) -> bool {
        self.workspace_switch
            .as_ref()
            .is_some_and(|s| s.is_animation_ongoing())
            || self
                .bar_highlight
                .borrow()
                .as_ref()
                .is_some_and(|highlight| !highlight.anim.is_done())
            || {
                let reveal = self.bar_reveal.borrow();
                reveal.anim.is_some() || reveal.hide_at.is_some()
            }
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
        let mut insert_hint_ws_geo = None;
        let insert_hint_ws_id = self
            .insert_hint
            .as_ref()
            .and_then(|hint| hint.workspace.existing_id());

        for ws in &mut self.workspaces {
            ws.update_render_elements(is_active, RenderLayer::MovingBetweenWorkspaces);
        }

        let card_radius = self.workspace_card_radius();
        for (ws, geo) in self.workspaces_with_render_geo_mut(true) {
            ws.set_card_radius(card_radius);
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
        set_caption_font(&options.layout.titlebar.font);

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

    #[cfg(test)]
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

    pub fn workspaces_render_geo(&self) -> impl Iterator<Item = Rectangle<f64, Logical>> {
        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();

        let ws_size = self.workspace_size(zoom);
        let gap = self.workspace_gap(zoom);
        let ws_height_with_gap = ws_size.h + gap;

        let mut static_offset = (self.view_size.to_point() - ws_size.to_point()).downscale(2.);
        // Workspace cards shrink towards the middle of the working area, so they
        // centre beside panels such as a side bar rather than under them.
        if self.options.overview.workspace_cards {
            let area = self.working_area;
            let pivot_x = area.loc.x + area.size.w / 2.;
            static_offset.x = pivot_x * (1. - ws_size.w / self.view_size.w.max(1.));
        }
        let static_offset = static_offset
            .to_physical_precise_round(scale)
            .to_logical(scale);

        let mut first_ws_y = -self.workspace_render_idx() * ws_height_with_gap;
        // Workspace cards keep the whole stack in view: centred when every workspace
        // fits below its label, otherwise scrolled to the active one without leaving
        // empty space past the first or last workspace.
        if let Some(progress) = self.workspace_cards_progress() {
            let count = self.workspaces.len().max(1) as f64;
            let total = count * ws_size.h + (count - 1.) * gap;
            let top = WORKSPACE_LABEL_HEIGHT + 16.;
            let bottom = 16.;
            let available = self.view_size.h - top - bottom;
            let centred = first_ws_y + static_offset.y;
            let target = if total <= available {
                top + (available - total) / 2.
            } else {
                centred.clamp(self.view_size.h - bottom - total, top)
            };
            first_ws_y += (target - centred) * progress;
        }
        let first_ws_y = round_logical_in_physical(scale, first_ws_y);

        // Return position for one-past-last workspace too.
        (0..=self.workspaces.len()).map(move |idx| {
            let y = first_ws_y + idx as f64 * ws_height_with_gap;
            let loc = Point::from((0., y)) + static_offset;

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
        self.workspaces_with_render_geo()
            .find_map(|(ws, geo)| geo.contains(pos_within_output).then_some(ws))
    }

    pub fn window_under(&self, pos_within_output: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // The compositor's own controls drawn over the windows take the pointer, so the
        // titlebar buttons beneath them do not react.
        if self.overlay_contains(pos_within_output) {
            return None;
        }

        let (ws, geo) = self.workspace_under(pos_within_output)?;

        if self.overview_progress.is_some() {
            let zoom = self.overview_zoom();
            let pos_within_workspace = (pos_within_output - geo.loc).downscale(zoom);
            let (win, hit) = ws.window_under(pos_within_workspace)?;
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
        if self.window_bar_available() {
            // Partly visible columns are cut one gap in from the panels, so their visible part
            // keeps the same margin as the other windows.
            let gaps = ws.options.layout.gaps;
            let span = bounds.start + gaps..bounds.end - gaps;
            return Some(span);
        }
        if ws.id() == self.active_workspace_ref().id() {
            return Some(Self::exclude_narrow_peeks(bounds, &self.scroll_cues()));
        }
        Some(bounds)
    }

    /// Tiles of the active workspace that lie mostly beyond the panels, with how strongly to
    /// dim them.
    fn peek_scrims(&self) -> Vec<(Rectangle<f64, Logical>, f32, bool, bool)> {
        let ws = self.active_workspace_ref();
        if self.workspace_switch.is_some()
            || self.overview_progress.is_some()
            || !self.window_bar_available()
        {
            return Vec::new();
        }
        let Some(bounds) = self.scrolling_visible_span(ws) else {
            return Vec::new();
        };
        // While a floating window has focus, the tiled windows recede a little, like the
        // backdrop of a dialog.
        let modal = ws.floating_is_active() && ws.is_floating_visible();
        ws.scrolling()
            .tiles_with_render_positions()
            .filter(|(_, _, visible)| *visible)
            .filter_map(|(tile, pos, _)| {
                let size = tile.tile_size();
                if size.w <= 0. {
                    return None;
                }
                let shown = (pos.x + size.w).min(bounds.end) - pos.x.max(bounds.start);
                let fraction = (shown / size.w).clamp(0., 1.);
                // Every partly hidden window recedes by the same amount; the ramp only
                // smooths the moment it starts to leave.
                let strength = ((1. - fraction) / 0.06).clamp(0., 1.) as f32;
                let strength = if modal { strength.max(0.35) } else { strength };
                // A window fading out at the edge takes its shade with it.
                (strength > 0.).then(|| {
                    let loc = self.workspace_origin(ws) + pos + tile.bob_offset();
                    // Only the visible part; the clipped side ends square like the window.
                    let left = loc.x.max(bounds.start);
                    let right = (loc.x + size.w).min(bounds.end);
                    let rect = Rectangle::new(
                        Point::from((left, loc.y)),
                        Size::from(((right - left).max(0.), size.h)),
                    );
                    let clipped_left = loc.x < bounds.start - 0.5;
                    let clipped_right = loc.x + size.w > bounds.end + 0.5;
                    (rect, strength, clipped_left, clipped_right)
                })
            })
            .collect()
    }

    fn render_peek_scrims<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        ws: &Workspace<W>,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        if !BorderRenderElement::has_shader(renderer) {
            return;
        }
        let scrims = self.peek_scrims();
        let mut elements = self.peek_scrims.borrow_mut();
        let count = scrims.len().max(elements.len());
        elements.resize_with(count, BorderRenderElement::empty);
        let backdrop = ws.options.layout.background_color;
        let scale = self.scale.fractional_scale() as f32;
        let radius = ws
            .scrolling()
            .tiles()
            .next()
            .map(|tile| {
                tile.window()
                    .rules()
                    .geometry_corner_radius
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let light = 0.2126 * backdrop.r + 0.7152 * backdrop.g + 0.0722 * backdrop.b >= 0.55;
        // Darken in both schemes; lightening reads as glare on pale windows.
        // Windows sliding past stay as they are; only a floating window's backdrop recedes.
        let (shade, amount) = if light { (0.06, 0.10) } else { (0., 0.18) };
        let modal = ws.floating_is_active() && ws.is_floating_visible();
        let amount = if modal { amount } else { 0. };
        let scale_f64 = self.scale.fractional_scale();
        // A short fade inside the clipped edge, so the window reads as continuing past it.
        for (index, (rect, strength, clipped_left, clipped_right)) in scrims
            .iter()
            .filter(|(rect, _, left, right)| rect.size.w >= 1. && (*left || *right))
            .take(2)
            .enumerate()
        {
            let (at_left, at_right) = (*clipped_left, *clipped_right);
            if !(at_left || at_right) || rect.size.w < 8. {
                continue;
            }
            let width = rect.size.w.min(24.);
            let x = if at_left {
                rect.loc.x
            } else {
                rect.loc.x + rect.size.w - width
            };
            if let Some(fade) = self.scroll_edge_fades[index].render(
                renderer.as_gles_renderer(),
                String::new(),
                Color::new_unpremul(shade, shade, shade, 1.),
                LabelKind::EdgeFade {
                    height: rect.size.h.round().max(1.) as u32,
                    from_left: at_left,
                },
                width,
                scale_f64,
                Point::from((x, rect.loc.y)),
                0.3 * strength,
            ) {
                push(RelocateRenderElement::from_element(
                    RescaleRenderElement::from_element(
                        MonitorInnerRenderElement::Caption(fade),
                        Point::default(),
                        1.,
                    ),
                    Point::default(),
                    Relocate::Relative,
                ));
            }
        }
        // Dimming as textures: the border shader cannot darken with a translucent fill.
        let mut labels = self.peek_scrim_labels.borrow_mut();
        let visible: Vec<_> = scrims
            .into_iter()
            .filter(|(rect, ..)| rect.size.w >= 1.)
            .collect();
        let count = visible.len().max(labels.len());
        labels.resize_with(count, PreviewLabel::default);
        for ((rect, strength, clipped_left, clipped_right), label) in
            visible.into_iter().zip(labels.iter())
        {
            // Square only where the window is actually cut.
            let (round_left, round_right) = (!clipped_left, !clipped_right);
            if let Some(scrim) = label.render_with_ink(
                renderer.as_gles_renderer(),
                String::new(),
                String::new(),
                Color::new_unpremul(shade, shade, shade, amount),
                Color::new_unpremul(shade, shade, shade, 1.),
                LabelKind::Scrim {
                    height: rect.size.h.round().max(1.) as u32,
                    round_left,
                    round_right,
                },
                rect.size.w,
                scale_f64,
                rect.loc,
                strength,
            ) {
                push(RelocateRenderElement::from_element(
                    RescaleRenderElement::from_element(
                        MonitorInnerRenderElement::Caption(scrim),
                        Point::default(),
                        1.,
                    ),
                    Point::default(),
                    Relocate::Relative,
                ));
            }
        }
        let _ = (elements, radius, scale);
    }

    fn workspace_origin(&self, ws: &Workspace<W>) -> Point<f64, Logical> {
        self.workspaces_with_render_geo()
            .find(|(candidate, _)| candidate.id() == ws.id())
            .map_or(Point::default(), |(_, geo)| geo.loc)
    }

    /// Whether the window bar arranges this output: the columns then slide out at the edges
    /// and its arrows reach the windows out of view, even while a bar shown on demand is
    /// hidden.
    fn window_bar_available(&self) -> bool {
        let layout = &self.options.layout;
        layout.clip_scroll_to_work_area
            && layout.window_bar != WindowBar::Off
            && navigation_strip_fits(self.working_area)
    }

    /// The top of the row reserved for the window bar above the columns.
    fn bar_row_top(&self) -> Option<f64> {
        (self.window_bar_available() && self.options.layout.window_bar == WindowBar::Always)
            .then_some(self.working_area.loc.y)
    }

    /// Where the window bar is drawn, while it is shown.
    fn navigation_strip_top(&self) -> Option<f64> {
        if !self.window_bar_available() {
            return None;
        }
        let top = self.working_area.loc.y;
        if self.options.layout.window_bar == WindowBar::Always {
            return Some(top);
        }
        // Shown on demand, it slides down from above the work area, over the windows.
        let progress = self.bar_reveal.borrow().progress();
        (progress > 0.).then(|| top - (1. - progress) * (NAVIGATION_STRIP_HEIGHT + 12.))
    }

    fn set_bar_shown(&self, shown: bool, by_key: bool) -> bool {
        let mut reveal = self.bar_reveal.borrow_mut();
        reveal.by_key = shown && by_key;
        reveal.hide_at = None;
        if reveal.shown == shown {
            return false;
        }
        let from = reveal.progress();
        reveal.shown = shown;
        reveal.anim = Some(Animation::new(
            self.clock.clone(),
            from,
            if shown { 1. } else { 0. },
            0.,
            self.options.animations.horizontal_view_movement.0,
        ));
        true
    }

    /// Shows or hides the window bar shown on demand. Returns whether it changed.
    pub fn toggle_window_bar(&self) -> bool {
        if self.options.layout.window_bar != WindowBar::OnDemand || !self.window_bar_available() {
            return false;
        }
        let shown = self.bar_reveal.borrow().shown;
        self.set_bar_shown(!shown, true)
    }

    /// Shows the window bar shown on demand while keyboard navigation moves between windows,
    /// and hides it again shortly after the last move. A bar shown with the shortcut or the
    /// pointer stays as it is.
    pub fn reveal_window_bar_for_navigation(&self) -> bool {
        if self.options.layout.window_bar != WindowBar::OnDemand || !self.window_bar_available() {
            return false;
        }
        let (shown, held) = {
            let reveal = self.bar_reveal.borrow();
            (reveal.shown, reveal.shown && reveal.hide_at.is_none())
        };
        if held {
            return false;
        }
        let changed = !shown && self.set_bar_shown(true, false);
        self.bar_reveal.borrow_mut().hide_at =
            Some(self.clock.now_unadjusted() + NAVIGATION_BAR_LINGER);
        changed
    }

    /// Hides a window bar shown on demand once it served its purpose: a window was chosen.
    pub fn hide_window_bar(&self) -> bool {
        self.options.layout.window_bar == WindowBar::OnDemand && self.set_bar_shown(false, false)
    }

    /// Reveals the window bar shown on demand when the pointer touches the top edge of the work
    /// area, and hides it once the pointer moves well below it. Returns whether it changed.
    pub fn update_bar_reveal_pointer(&self, position: Option<Point<f64, Logical>>) -> bool {
        if self.options.layout.window_bar != WindowBar::OnDemand || !self.window_bar_available() {
            return self.set_bar_shown(false, false);
        }
        let (shown, by_key) = {
            let reveal = self.bar_reveal.borrow();
            (reveal.shown, reveal.by_key)
        };
        if by_key {
            return false;
        }
        let area = self.working_area;
        let Some(position) = position else {
            return shown && self.set_bar_shown(false, false);
        };
        let within = (area.loc.x..area.loc.x + area.size.w).contains(&position.x);
        // The top edge of the screen, or of the work area when a panel sits above it.
        let at_edge = within
            && (position.y <= 0.5 || (position.y - area.loc.y).abs() <= 1.)
            && self.workspace_switch.is_none()
            && self.overview_progress.is_none();
        if !shown && at_edge {
            return self.set_bar_shown(true, false);
        }
        let below = position.y > area.loc.y + NAVIGATION_STRIP_HEIGHT + 24.;
        if shown && (below || !within) {
            return self.set_bar_shown(false, false);
        }
        if shown {
            // The pointer on a bar shown for keyboard navigation keeps it.
            self.bar_reveal.borrow_mut().hide_at = None;
        }
        false
    }

    fn scrolling_clip_excludes(
        &self,
        ws: &Workspace<W>,
        workspace_origin: Point<f64, Logical>,
        location: Point<f64, Logical>,
    ) -> bool {
        self.scrolling_visible_span(ws).is_some_and(|bounds| {
            !bounds.contains(&location.x)
                || self.bar_row_top().is_some_and(|top| {
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
        let Some(bounds) = self.cue_bounds(ws) else {
            return [None, None];
        };
        self.scroll_cues_for(ws, &bounds)
    }

    /// The span the scroll arrows mark the edges of: between the panels, and between the
    /// pinned columns in view, since columns scroll under those too.
    fn cue_bounds(&self, ws: &Workspace<W>) -> Option<Range<f64>> {
        let bounds = self.scroll_clip(ws)?;
        let area = ws.scrolling().working_area();
        let start = bounds.start.max(area.loc.x);
        let end = bounds.end.min(area.loc.x + area.size.w);
        (start < end).then_some(start..end)
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
        // Pinned columns are always in view; the others can scroll under them.
        let columns = || {
            scrolling
                .columns_with_render_positions()
                .filter(|(column, _)| column.id() != active.id() && !column.is_pinned_in_place())
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
        let make_cue = |(column, pos): (&'a Column<W>, Point<f64, Logical>),
                        x,
                        width: f64,
                        left| {
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
            // The large card for a clipped column belongs to layouts without the window bar;
            // with it, the slim handle and its pill do the job.
            let can_clip = card_progress > 0. && unclipped && !self.window_bar_available();
            let card = can_clip
                && width >= 144.
                && self.working_area.size.h >= SCROLL_CARD_HEIGHT
                && exposed < column.width() * 0.60;
            // With the strip available, retract the last part of a clipped
            // column before its titlebar buttons become detached slivers.
            let peek_width = (column.width() * 0.30).min(168.);
            let strip_peek =
                self.window_bar_available() && unclipped && exposed > 0. && exposed < peek_width;
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
            let top = self.working_area.loc.y;
            // An arrow comes out beside the pointer as it nears its edge, and stays out of the
            // way otherwise.
            let pointer = self
                .cue_pointer
                .get()
                .filter(|point| point.y >= top && point.y <= top + self.working_area.size.h);
            let reach = pointer.map_or(0., |point| {
                let distance = if left {
                    point.x - bounds.start
                } else {
                    bounds.end - point.x
                };
                ((CUE_REACH - distance) / (CUE_REACH - CUE_FULL_REACH)).clamp(0., 1.)
            });
            let armed_at = (!card)
                .then(|| self.cue_armed.get()[usize::from(!left)])
                .flatten();
            let y = if card {
                top + available * 0.30
            } else if let Some(at) = armed_at {
                (at - height / 2.).clamp(top + inset, (top + available - inset).max(top))
            } else if let Some(point) = pointer {
                (point.y - height / 2.).clamp(top + inset, (top + available - inset).max(top))
            } else {
                top + (available - inset).max(0.)
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
            // Until the pointer reaches it through the gap beside its edge, the arrow is a slim
            // handle in that gap, clear of the windows, so it never takes their clicks.
            let armed = armed_at.is_some();
            let rect = if card {
                rect
            } else if armed {
                // Open, it reaches the edge, so it can be clicked anywhere from the edge to its
                // label.
                let (start, end) = if left {
                    (bounds.start, rect.loc.x + rect.size.w)
                } else {
                    (rect.loc.x, bounds.end)
                };
                Rectangle::new(
                    Point::from((start, rect.loc.y)),
                    Size::from((end - start, rect.size.h)),
                )
            } else {
                let gaps = ws.options.layout.gaps;
                let tab = (gaps - 6.).clamp(4., 10.);
                let gutter = if left {
                    bounds.start
                } else {
                    bounds.end - gaps
                };
                Rectangle::new(
                    Point::from((gutter + (gaps - tab) / 2., y)),
                    Size::from((tab, height)),
                )
            };
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
                // With the window bar, partly visible columns stay in view.
                clipped_column_edge: (clip_progress > 0. && !self.window_bar_available()).then(
                    || {
                        let boundary = if left { bounds.start } else { bounds.end };
                        boundary + (edge - boundary) * clip_progress
                    },
                ),
                opacity: if card {
                    card_progress as f32
                } else if armed {
                    1.
                } else {
                    reach as f32
                },
                armed,
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
        .map(|cue| {
            // Arrows belong to the tiled windows: shown floating windows cover them.
            cue.filter(|cue| cue.opacity > 0. && !self.covered_by_floating(ws, cue.rect))
        })
    }

    /// Normal tiled windows leave a row for the strip, so the same controls
    /// remain available in each position without painting over a client surface.
    fn navigation_strip(&self) -> Option<NavigationStrip<'_, W>> {
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
        let visible = bounds.clone();
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
        let tiled_count = windows.len();
        let mut floating = ws
            .floating()
            .tiles_with_render_positions()
            .filter(|(tile, _)| !tile.is_hidden())
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
        let scratchpad = self.scratchpad_count.get() > 0;
        if windows.len() == 1 && floating.is_empty() && !scratchpad {
            return None;
        }
        let floating_visible = ws.is_floating_visible() && !floating.is_empty();
        // The bar's top lines up with the rail's: both sit 12 px from the screen edge.
        let tray_y = top + NAVIGATION_STRIP_HEIGHT - BAR_TRAY_HEIGHT;
        // The bar stays clear of floating windows that reach into its row.
        let row = Rectangle::new(
            Point::from((bounds.start, tray_y)),
            Size::from((bounds.end - bounds.start, BAR_TRAY_HEIGHT)),
        );
        // Shown on demand, the bar is called up over the windows, floating ones included.
        if floating_visible
            && self.options.layout.window_bar == WindowBar::Always
            && floating.iter().any(|(tile, position)| {
                row.intersection(Rectangle::new(*position, tile.tile_size()))
                    .is_some()
            })
        {
            return None;
        }
        let edge_targets = [
            windows
                .iter()
                .rfind(|(_, direction, ..)| *direction == Some(true)),
            windows
                .iter()
                .find(|(_, direction, ..)| *direction == Some(false)),
        ]
        .map(|target| target.map(|(window, ..)| *window));

        let active_index = windows
            .iter()
            .position(|(window, ..)| window.id() == active.id())?;
        let layer_destination = match (scrolling.active_window(), ws.floating().active_window()) {
            (Some(tiled), Some(floating)) => Some(if floating_visible { tiled } else { floating }),
            _ => None,
        };

        let gap = BAR_PILL_GAP;
        let pad = (BAR_TRAY_HEIGHT - BAR_PILL_HEIGHT) / 2.;
        let button = BAR_PILL_HEIGHT;
        // Each arrow carries the number of windows out of view on its side; with nothing out
        // of view, there are no arrows.
        let counts = [Some(true), Some(false)].map(|side| {
            windows[..tiled_count]
                .iter()
                .filter(|(_, direction, ..)| *direction == side)
                .count()
        });
        let show_arrows = tiled_count >= 2 && counts.iter().any(|count| *count > 0);
        let arrow_width =
            // Room for a "+N" badge beside the chevron.
            |count: usize| {
                button
                    + if count > 0 {
                        12. + 7. * (count.to_string().len() + 1) as f64
                    } else {
                        0.
                    }
            };
        let arrow_widths = counts.map(arrow_width);
        let arrow_space = if show_arrows {
            arrow_widths[0] + arrow_widths[1] + 2. * gap
        } else {
            0.
        };
        // The floating layer's control is a round icon button, set apart from the windows. The
        // scratchpad's button follows it while the scratchpad holds windows.
        let toggles = usize::from(layer_destination.is_some()) + usize::from(scratchpad);
        let toggle_width =
            toggles as f64 * BAR_PILL_HEIGHT + toggles.saturating_sub(1) as f64 * gap;
        let toggle_space = if toggles > 0 { toggle_width + gap } else { 0. };
        let divider_space = 2. * BAR_DIVIDER_GAP + 1. - gap;
        // Each name gets the width of its label, within limits; the bar grows with them up to
        // a share of the output, then the names shrink and finally the bar shows fewer windows.
        let natural = windows
            .iter()
            .map(|&(window, _, _, is_floating, _, _)| {
                let stacked = if is_floating {
                    1
                } else {
                    scrolling
                        .columns()
                        .find(|column| column.contains(window.id()))
                        .map_or(1, |column| column.tiles().count())
                };
                let has_icon = window.with_app_id(|app_id| {
                    crate::app_icons::icon(
                        app_id,
                        (16. * self.scale.fractional_scale()).round() as i32,
                    )
                    .is_some()
                });
                window
                    .with_decoration_title(|title| {
                        measure_bar_label(title, has_icon, is_floating, stacked)
                    })
                    .clamp(BAR_PILL_MIN_WIDTH, BAR_PILL_MAX_WIDTH)
            })
            .collect::<Vec<_>>();
        let max_width = ((bounds.end - bounds.start) * BAR_MAX_SHARE)
            .min(bounds.end - bounds.start - 2. * BAR_MARGIN);
        let fixed = 2. * pad + arrow_space + toggle_space;
        let row_width = |range: Range<usize>| {
            let has_tiled = windows[range.clone()].iter().any(|window| !window.3);
            let has_floating = windows[range.clone()].iter().any(|window| window.3);
            let divider = if has_tiled && has_floating {
                divider_space
            } else {
                0.
            };
            natural[range.clone()].iter().sum::<f64>() + gap * (range.len() as f64 - 1.) + divider
        };
        let mut count = windows.len().min(MAX_NAVIGATION_TITLES);
        let first = |count: usize| {
            active_index
                .saturating_sub(count / 2)
                .min(windows.len() - count)
        };
        // Fewer windows only once every name is at its minimum width.
        while count > 1 {
            let start = first(count);
            let min_row = BAR_PILL_MIN_WIDTH * count as f64 + gap * (count as f64 - 1.);
            let has_both = windows[start..start + count].iter().any(|w| w.3)
                && windows[start..start + count].iter().any(|w| !w.3);
            let min_row = min_row + if has_both { divider_space } else { 0. };
            if fixed + min_row <= max_width {
                break;
            }
            count -= 1;
        }
        let first = first(count);
        let shown = &windows[first..first + count];
        let shown_tiled = shown.iter().filter(|window| !window.3).count();
        let shown_floating = count - shown_tiled;
        let row = row_width(first..first + count);
        let available_row = max_width - fixed;
        // Shrink the wider names first, keeping each at least at the minimum.
        let widths: Vec<f64> = if row <= available_row {
            natural[first..first + count].to_vec()
        } else {
            let slack = |w: f64| (w - BAR_PILL_MIN_WIDTH).max(0.);
            let total_slack: f64 = natural[first..first + count]
                .iter()
                .map(|w| slack(*w))
                .sum();
            let excess = row - available_row;
            let factor = if total_slack > 0. {
                (1. - excess / total_slack).clamp(0., 1.)
            } else {
                0.
            };
            natural[first..first + count]
                .iter()
                .map(|w| BAR_PILL_MIN_WIDTH + slack(*w) * factor)
                .collect()
        };
        // Tiled windows sit in the main bar; floating windows get their own small bar beside it,
        // so neither moves when the other changes.
        let row =
            |widths: &[f64]| widths.iter().sum::<f64>() + gap * (widths.len().max(1) as f64 - 1.);
        let natural_tiled = if shown_tiled > 0 {
            2. * pad + arrow_space + row(&widths[..shown_tiled])
        } else {
            0.
        };
        // A minimum width keeps the arrows still as windows open and close.
        let tiled_width = if shown_tiled > 0 {
            natural_tiled.max(BAR_MIN_WIDTH.min(bounds.end - bounds.start - 2. * BAR_MARGIN))
        } else {
            0.
        };
        let tiled_inset = (tiled_width - natural_tiled) / 2.;
        let floating_width = if shown_floating > 0 || toggles > 0 {
            let pills = if shown_floating > 0 {
                row(&widths[shown_tiled..]) + gap
            } else {
                0.
            };
            2. * pad + pills + toggle_width
        } else {
            0.
        };
        // Floating windows get their own small bar beside the main one.
        let separation = if tiled_width > 0. && floating_width > 0. {
            BAR_GROUP_GAP
        } else {
            0.
        };
        let total = tiled_width + separation + floating_width;
        // The main bar stays centred; the floating bar hangs off its right end, so the tiled
        // windows keep their place when a window starts or stops floating. Only when the pair
        // would not fit does the main bar give way.
        // The main bar stays centred and anchored; the floating bar hangs off its right end.
        let centre = if tiled_width > 0. { tiled_width } else { total };
        let tray_x = ((bounds.start + bounds.end - centre) / 2.)
            .min(bounds.end - BAR_MARGIN - total)
            .max(bounds.start);
        let floating_x = tray_x + tiled_width + separation;
        let tray_width = tiled_width;
        let y = tray_y + pad;
        let rect = |x: f64, width: f64| {
            Rectangle::new(Point::from((x, y)), Size::from((width, BAR_PILL_HEIGHT)))
        };

        let left_arrow = show_arrows.then(|| rect(tray_x + pad, arrow_widths[0]));
        let right_arrow =
            show_arrows.then(|| rect(tray_x + tray_width - pad - arrow_widths[1], arrow_widths[1]));
        let toggles_x = floating_x + floating_width - pad - toggle_width;
        let layer_action =
            layer_destination.map(|window| (rect(toggles_x, BAR_PILL_HEIGHT), window));
        let scratchpad_action = scratchpad.then(|| {
            rect(
                floating_x + floating_width - pad - BAR_PILL_HEIGHT,
                BAR_PILL_HEIGHT,
            )
        });
        let mut x = tray_x
            + tiled_inset
            + pad
            + if show_arrows {
                arrow_widths[0] + gap
            } else {
                0.
            };
        let mut in_floating_bar = false;
        let mut titles = Vec::with_capacity(count);
        for (&(window, _, fully_visible, is_floating, _, _), &pill_width) in zip(shown, &widths) {
            if is_floating && !in_floating_bar {
                in_floating_bar = true;
                x = floating_x + pad;
            }
            let pill = rect(x, pill_width);
            let is_active = window.id() == active.id();
            titles.push(NavigationTitle {
                rect: pill,
                window,
                active: is_active,
                fully_visible,
                floating: is_floating,
            });
            x += pill_width + gap;
        }
        let edge_actions = [
            left_arrow.map(|rect| NavigationArrow {
                rect,
                target: edge_targets[0],
                count: counts[0],
            }),
            right_arrow.map(|rect| NavigationArrow {
                rect,
                target: edge_targets[1],
                count: counts[1],
            }),
        ];
        let tray = (tray_width > 0.).then(|| {
            Rectangle::new(
                Point::from((tray_x, tray_y)),
                Size::from((tray_width, BAR_TRAY_HEIGHT)),
            )
        });
        let floating_tray = (floating_width > 0.).then(|| {
            Rectangle::new(
                Point::from((floating_x, tray_y)),
                Size::from((floating_width, BAR_TRAY_HEIGHT)),
            )
        });
        Some(NavigationStrip {
            tray,
            moving,
            edge_actions,
            titles,
            layer_action,
            scratchpad_action,
            floating_tray,
        })
    }

    /// Sets how many windows the scratchpad holds, which the window bar shows.
    pub(super) fn set_scratchpad_count(&self, count: usize) {
        self.scratchpad_count.set(count);
    }

    /// Whether `position` is on the window bar's scratchpad button.
    pub fn scratchpad_button_under(&self, position: Point<f64, Logical>) -> bool {
        !self.overview_open
            && self
                .navigation_strip()
                .and_then(|strip| strip.scratchpad_action)
                .is_some_and(|rect| rect.contains(position))
    }

    pub fn navigation_window_under(&self, position: Point<f64, Logical>) -> Option<&W> {
        if self.overview_open {
            return None;
        }
        if let Some(tab) = self
            .folded_tabs()
            .into_iter()
            .find(|tab| tab.hit_area(self.folded_hover.get()).contains(position))
        {
            return Some(tab.window);
        }

        if let Some(strip) = self.navigation_strip() {
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

    pub fn dismiss_navigation_preview(&self, position: Point<f64, Logical>) {
        let arrow = self.navigation_strip().and_then(|strip| {
            strip
                .edge_actions
                .into_iter()
                .flatten()
                .find(|arrow| arrow.rect.contains(position))
                .map(|arrow| arrow.rect)
        });
        self.dismissed_preview_arrow.set(arrow);
    }

    /// Which scroll arrows open for the pointer at `position`: an arrow opens when the pointer
    /// enters the gap between its edge and the windows, and stays open while the pointer is
    /// on it.
    fn cue_arming(&self, position: Option<Point<f64, Logical>>) -> [Option<f64>; 2] {
        let Some(point) = position else {
            return [None; 2];
        };
        let ws = self.active_workspace_ref();
        let Some(bounds) = self.cue_bounds(ws) else {
            return [None; 2];
        };
        let area = self.working_area;
        if point.y < area.loc.y || point.y > area.loc.y + area.size.h {
            return [None; 2];
        }
        // A folded pinned column's tab takes the pointer before the arrows do.
        if self.folded_hover.get().is_some() {
            return [None; 2];
        }
        let gutter = ws.options.layout.gaps.max(8.);
        let current = self.cue_armed.get();
        let cues = self.scroll_cues();
        std::array::from_fn(|side| {
            let Some(cue) = &cues[side] else {
                return None;
            };
            let in_gap = if side == 0 {
                (bounds.start - 1.0..=bounds.start + gutter).contains(&point.x)
            } else {
                (bounds.end - gutter..=bounds.end + 1.).contains(&point.x)
            };
            if in_gap {
                // In the gap, it follows the pointer up and down.
                Some(point.y)
            } else {
                current[side].filter(|_| cue.rect.contains(point))
            }
        })
    }

    pub fn update_navigation_preview_pointer(&self, position: Option<Point<f64, Logical>>) {
        self.cue_pointer.set(position);
        let hover = self.folded_hover.get();
        let hover = position.and_then(|point| {
            self.folded_tabs()
                .into_iter()
                .find(|tab| tab.hit_area(hover).contains(point))
                .map(|tab| tab.side)
        });
        self.folded_hover.set(hover);
        self.cue_armed.set(self.cue_arming(position));
        if self
            .dismissed_preview_arrow
            .get()
            .is_some_and(|arrow| position.is_none_or(|point| !arrow.contains(point)))
        {
            self.dismissed_preview_arrow.set(None);
        }
    }

    pub fn window_has_navigation_caption(&self, id: &W::Id) -> bool {
        self.navigation_strip()
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

    /// The popover under the window bar that previews `window`, centered under `anchor`.
    fn bar_preview_card(
        &self,
        anchor: Rectangle<f64, Logical>,
        tray: Rectangle<f64, Logical>,
        window: &W,
    ) -> Option<Rectangle<f64, Logical>> {
        let source = window.size().to_f64();
        if source.w <= 0. || source.h <= 0. {
            return None;
        }
        let area = self.working_area;
        // Fit the window into at most 320 × 260, and the card snugly around it.
        let fit = (320. / source.w).min(260. / source.h).min(1.);
        let thumbnail = source.upscale(fit);
        let width = (thumbnail.w + 2. * BAR_CARD_PADDING)
            .max(240.)
            .min(area.size.w - 2. * BAR_MARGIN);
        let height = BAR_CARD_TITLE_HEIGHT + thumbnail.h + BAR_CARD_PADDING;
        let top = tray.loc.y + tray.size.h + 10.;
        if width < 160. || top + height > area.loc.y + area.size.h {
            return None;
        }
        let left = (anchor.loc.x + (anchor.size.w - width) / 2.).clamp(
            area.loc.x + BAR_MARGIN,
            (area.loc.x + area.size.w - BAR_MARGIN - width).max(area.loc.x + BAR_MARGIN),
        );
        let scale = self.scale.fractional_scale();
        let rect = Rectangle::new(Point::from((left, top)), Size::from((width, height)));
        Some(Rectangle::new(
            rect.loc.to_physical_precise_round(scale).to_logical(scale),
            rect.size,
        ))
    }

    /// Renders the whole of `window`, scaled to fit and centered in `rect`.
    fn render_window_thumbnail<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
        rect: Rectangle<f64, Logical>,
        window: &W,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        if target.should_block_out(window.rules().block_out_from) {
            return;
        }
        let source = window.size().to_f64();
        if source.w <= 0. || source.h <= 0. {
            return;
        }
        let scale = self.scale.fractional_scale();
        let zoom = (rect.size.w / source.w).min(rect.size.h / source.h).min(1.);
        let shown = source.upscale(zoom);
        let loc = rect.loc + (rect.size.to_point() - shown.to_point()).downscale(2.);
        let location = loc.to_physical_precise_round(scale);
        let crop = Rectangle::from_size(source.to_physical_precise_round(scale));
        let ctx = RenderCtx {
            renderer,
            target,
            xray: None,
        };
        window.render_normal(ctx, Point::default(), scale.into(), 1., &mut |elem| {
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

    fn render_scroll_cues<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
        pointer: Option<Point<f64, Logical>>,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        // While the window bar slides in or out, its entries pass under a still pointer; they
        // only react to it once the bar has settled, so previews do not flicker.
        let pointer = pointer.filter(|_| self.bar_reveal.borrow().anim.is_none());
        let preview_pointer = pointer.filter(|point| {
            self.dismissed_preview_arrow
                .get()
                .is_none_or(|arrow| !arrow.contains(*point))
        });
        if let Some(strip) = self.navigation_strip() {
            let ws = self.active_workspace_ref();
            let accent = ws.options.layout.border.active_color;
            let titlebar = &ws.options.layout.titlebar;
            // Opaque, like the rail beside it.
            let surface = Color {
                a: 1.,
                ..titlebar.background_color(false)
            };
            let ink = titlebar.text_color(true);
            let scale = self.scale.fractional_scale();
            let floating_visible = ws.is_floating_visible() && !ws.floating().is_empty();
            let moving = strip.moving;
            let windows = ws.windows().collect::<Vec<_>>();
            let titles = windows
                .iter()
                .map(|window| navigation_title(*window, target, "Window"))
                .collect::<Vec<_>>();
            let display_title = |window: &W| {
                windows
                    .iter()
                    .position(|candidate| candidate.id() == window.id())
                    .map_or("Window", |index| titles[index].as_str())
            };
            // A popover previews the window that a hovered name or arrow brings into view.
            let preview = preview_pointer.filter(|_| !moving).and_then(|point| {
                strip
                    .titles
                    .iter()
                    .find(|title| title.rect.contains(point))
                    .filter(|title| !title.active && !title.fully_visible && !title.floating)
                    .map(|title| (title.rect, title.window))
                    .or_else(|| {
                        strip
                            .edge_actions
                            .iter()
                            .flatten()
                            .find(|arrow| arrow.rect.contains(point))
                            .and_then(|arrow| arrow.target.map(|window| (arrow.rect, window)))
                    })
            });
            let mut card = None;
            if let (Some((anchor, window)), Some(tray)) = (preview, strip.tray) {
                card = self.bar_preview_card(anchor, tray, window);
            }
            if let (Some((_, window)), Some(card)) = (preview, card) {
                let padding = BAR_CARD_PADDING;
                let thumbnail = Rectangle::new(
                    card.loc + Point::from((padding, BAR_CARD_TITLE_HEIGHT)),
                    Size::from((
                        card.size.w - 2. * padding,
                        card.size.h - BAR_CARD_TITLE_HEIGHT - padding,
                    )),
                );
                self.render_window_thumbnail(renderer, target, thumbnail, window, push);
                let margin = Point::from((BAR_SHADOW_MARGIN, BAR_SHADOW_MARGIN));
                if let Some(label) = self.strip_card.render_with_ink(
                    renderer.as_gles_renderer(),
                    display_title(window).to_owned(),
                    window.with_app_id(str::to_owned),
                    Color { a: 1., ..surface },
                    ink,
                    LabelKind::BarCard {
                        height: card.size.h.round() as u32,
                    },
                    card.size.w,
                    scale,
                    card.loc - margin,
                    1.,
                ) {
                    let elem = MonitorInnerRenderElement::Caption(label);
                    let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                    push(RelocateRenderElement::from_element(
                        elem,
                        Point::default(),
                        Relocate::Relative,
                    ));
                }
            }
            let mut draw = |elem: MonitorInnerRenderElement<R>| {
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                push(RelocateRenderElement::from_element(
                    elem,
                    Point::default(),
                    Relocate::Relative,
                ));
            };

            for (index, (edge, label)) in strip
                .edge_actions
                .iter()
                .zip(&self.strip_edge_labels)
                .enumerate()
            {
                let Some(arrow) = edge else { continue };
                let enabled = arrow.target.is_some();
                let hovered = enabled && pointer.is_some_and(|point| arrow.rect.contains(point));
                if hovered && arrow.count > 0 {
                    // Say what the number means, under the arrow.
                    let side = if index == 0 { "left" } else { "right" };
                    let text = if arrow.count == 1 {
                        format!("1 window to the {side}")
                    } else {
                        format!("{} windows to the {side}", arrow.count)
                    };
                    let width = 320.;
                    let y = arrow.rect.loc.y + arrow.rect.size.h + 14.;
                    let x = if index == 0 {
                        arrow.rect.loc.x
                    } else {
                        arrow.rect.loc.x + arrow.rect.size.w - width
                    };
                    if let Some(tip) = self.arrow_tooltips[index].render_with_ink(
                        renderer.as_gles_renderer(),
                        text,
                        String::new(),
                        Color { a: 1., ..surface },
                        ink,
                        LabelKind::Tooltip {
                            right_align: index == 1,
                        },
                        width,
                        scale,
                        Point::from((x, y)),
                        1.,
                    ) {
                        draw(MonitorInnerRenderElement::Caption(tip));
                    }
                }
                if let Some(element) = label.render_with_ink(
                    renderer.as_gles_renderer(),
                    if arrow.count > 0 {
                        arrow.count.to_string()
                    } else {
                        String::new()
                    },
                    String::new(),
                    Color { a: 0., ..surface },
                    ink,
                    LabelKind::BarButton {
                        icon: if index == 0 {
                            BarIcon::Left
                        } else {
                            BarIcon::Right
                        },
                        enabled,
                        hovered,
                    },
                    arrow.rect.size.w,
                    scale,
                    arrow.rect.loc,
                    1.,
                ) {
                    draw(MonitorInnerRenderElement::Caption(element));
                }
            }
            if let Some(rect) = strip.scratchpad_action {
                let hovered = pointer.is_some_and(|point| rect.contains(point));
                let shown = ws.scratchpad_windows().any(|(_, hidden)| !hidden);
                let count = self.scratchpad_count.get();
                if let Some(element) = self.scratchpad_label.render_with_ink(
                    renderer.as_gles_renderer(),
                    if count > 1 {
                        count.to_string()
                    } else {
                        String::new()
                    },
                    String::new(),
                    Color { a: 0., ..surface },
                    ink,
                    LabelKind::BarButton {
                        icon: BarIcon::Scratchpad { shown },
                        enabled: true,
                        hovered,
                    },
                    rect.size.w,
                    scale,
                    rect.loc,
                    1.,
                ) {
                    draw(MonitorInnerRenderElement::Caption(element));
                }
            }
            if let Some((rect, _)) = strip.layer_action {
                let hovered = pointer.is_some_and(|point| rect.contains(point));
                if let Some(element) = self.layer_action_label.render_with_ink(
                    renderer.as_gles_renderer(),
                    String::new(),
                    String::new(),
                    Color { a: 0., ..surface },
                    ink,
                    LabelKind::BarButton {
                        icon: if floating_visible {
                            BarIcon::HideFloating
                        } else {
                            BarIcon::ShowFloating
                        },
                        enabled: true,
                        hovered,
                    },
                    rect.size.w,
                    scale,
                    rect.loc,
                    1.,
                ) {
                    draw(MonitorInnerRenderElement::Caption(element));
                }
            }
            // The focus fill moves to a window as it arrives on screen, in step with the view,
            // rather than the moment focus changes.
            let active_id = ws.active_window().map(|window| window.id().clone());
            // The focus fill starts moving with focus and slides along with the view.
            let arrived = true
                || ws.floating_is_active()
                || active_id.as_ref().is_none_or(|id| {
                    let bounds = self.scroll_view_bounds();
                    ws.scrolling()
                        .tiles_with_render_positions()
                        .find(|(tile, _, _)| tile.window().id() == id)
                        .is_none_or(|(tile, pos, _)| {
                            let width = tile.tile_size().w.max(1.);
                            let shown = (pos.x + width).min(bounds.end) - pos.x.max(bounds.start);
                            shown >= width * 0.25
                        })
                });
            let shown_focus = {
                let mut last = self.bar_focus.borrow_mut();
                if arrived
                    || last
                        .as_ref()
                        .is_none_or(|id| !ws.windows().any(|w| w.id() == id))
                {
                    last.clone_from(&active_id);
                }
                last.clone()
            };
            let sliding = BorderRenderElement::has_shader(renderer);
            let (highlight_rect, styled_focus) = {
                let target = strip
                    .titles
                    .iter()
                    .find(|title| shown_focus.as_ref() == Some(title.window.id()));
                let mut highlight = self.bar_highlight.borrow_mut();
                match (target, shown_focus.as_ref()) {
                    (Some(target), Some(id)) => {
                        let current = highlight.as_ref().map(BarHighlight::rect);
                        let same = highlight.as_ref().is_some_and(|h| &h.window == id);
                        if same {
                            let h = highlight.as_mut().unwrap();
                            h.to = target.rect;
                            if h.anim.is_done() {
                                h.from = target.rect;
                            }
                        } else {
                            let previous = highlight.as_ref().map(|h| h.window.clone());
                            *highlight = Some(BarHighlight {
                                window: id.clone(),
                                previous,
                                from: current.unwrap_or(target.rect),
                                to: target.rect,
                                anim: Animation::new(
                                    self.clock.clone(),
                                    0.,
                                    1.,
                                    0.,
                                    self.options.animations.horizontal_view_movement.0,
                                ),
                            });
                        }
                        let h = highlight.as_ref().unwrap();
                        // Names take the focused style once the fill is halfway there.
                        let styled = if h.anim.clamped_value() < 0.5 {
                            h.previous.clone().or_else(|| Some(id.clone()))
                        } else {
                            Some(id.clone())
                        };
                        let _ = styled;
                        (Some((h.anchor(), h.fills(), h.sliding())), Some(id.clone()))
                    }
                    _ => {
                        *highlight = None;
                        (None, shown_focus.clone())
                    }
                }
            };
            let slide = highlight_rect.as_ref().and_then(|(_, _, slide)| *slide);
            for ((title, label), focus_label) in strip
                .titles
                .iter()
                .zip(&self.strip_labels)
                .zip(&self.strip_focus_labels)
            {
                // Where a sliding fill only partly covers a name, the name keeps its own style
                // and a focused copy shows inside the fill, so no letter loses its contrast.
                let partial = sliding
                    .then_some(slide)
                    .flatten()
                    .and_then(|fill| fill.intersection(title.rect))
                    .filter(|cover| cover.size.w > 0.5);
                // While the fill slides, a name takes the focused style when the fill covers
                // most of it, so its text always contrasts with what is behind it.
                let focused = if partial.is_some() {
                    false
                } else if sliding {
                    let _ = &styled_focus;
                    highlight_rect
                        .as_ref()
                        .map(|(anchor, _, _)| *anchor)
                        .is_some_and(|fill| {
                            let overlap = (fill.loc.x + fill.size.w)
                                .min(title.rect.loc.x + title.rect.size.w)
                                - fill.loc.x.max(title.rect.loc.x);
                            overlap >= title.rect.size.w.min(fill.size.w) * 0.5
                        })
                } else {
                    shown_focus.as_ref() == Some(title.window.id())
                };
                let hovered = pointer.is_some_and(|point| title.rect.contains(point));
                let referenced =
                    preview.is_some_and(|(_, window)| window.id() == title.window.id());
                // Windows mostly on screen get a faint tint, once the view has settled.
                let on_screen = !moving && !title.floating && {
                    let bounds = self.scroll_view_bounds();
                    ws.scrolling()
                        .tiles_with_render_positions()
                        .find(|(tile, _, _)| tile.window().id() == title.window.id())
                        .is_some_and(|(tile, pos, _)| {
                            let width = tile.tile_size().w.max(1.);
                            let shown = (pos.x + width).min(bounds.end) - pos.x.max(bounds.start);
                            shown >= width * 0.5
                        })
                };
                let state = if focused {
                    BarPillState::Focused
                } else if on_screen {
                    BarPillState::Visible
                } else {
                    BarPillState::Offscreen
                };
                // A window on screen carries a faint tint of the accent, passed as a clear
                // background; it stays neutral while a floating window holds the focus.
                let tinted = on_screen && !ws.floating_is_active();
                let background = if focused && !sliding {
                    accent
                } else if tinted {
                    Color { a: 0., ..accent }
                } else {
                    Color { a: 0., ..surface }
                };
                let pill_ink = if focused { caption_ink(accent) } else { ink };
                let stacked = ws
                    .scrolling()
                    .columns()
                    .find(|column| column.contains(title.window.id()))
                    .map_or(1, |column| column.tiles().count());
                if let Some(element) = label.render_with_ink(
                    renderer.as_gles_renderer(),
                    display_title(title.window).to_owned(),
                    title.window.with_app_id(str::to_owned),
                    background,
                    pill_ink,
                    LabelKind::BarPill {
                        state,
                        hovered: (hovered || referenced) && !moving,
                        floating: title.floating,
                        stacked: stacked.min(99) as u8,
                        tinted,
                    },
                    title.rect.size.w,
                    scale,
                    title.rect.loc,
                    if title.floating && !floating_visible {
                        0.6
                    } else {
                        1.
                    },
                ) {
                    let Some(cover) = partial else {
                        draw(MonitorInnerRenderElement::Caption(element));
                        continue;
                    };
                    let mut push_clipped =
                        |element: PrimaryGpuTextureRenderElement, rect: Rectangle<f64, Logical>| {
                            if rect.size.w <= 0. {
                                return;
                            }
                            let crop = rect.to_physical_precise_round(scale);
                            if let Some(element) =
                                CropRenderElement::from_element(element, scale, crop)
                            {
                                draw(MonitorInnerRenderElement::ClippedCaption(element));
                            }
                        };
                    // The focused copy inside the fill, the name's own style either side of it.
                    if let Some(focus) = focus_label.render_with_ink(
                        renderer.as_gles_renderer(),
                        display_title(title.window).to_owned(),
                        title.window.with_app_id(str::to_owned),
                        Color { a: 0., ..accent },
                        caption_ink(accent),
                        LabelKind::BarPill {
                            state: BarPillState::Focused,
                            hovered: false,
                            floating: title.floating,
                            stacked: stacked.min(99) as u8,
                            tinted: false,
                        },
                        title.rect.size.w,
                        scale,
                        title.rect.loc,
                        1.,
                    ) {
                        push_clipped(focus, cover);
                    }
                    let r = title.rect;
                    let before =
                        Rectangle::new(r.loc, Size::from((cover.loc.x - r.loc.x, r.size.h)));
                    let after_x = cover.loc.x + cover.size.w;
                    let after = Rectangle::new(
                        Point::from((after_x, r.loc.y)),
                        Size::from((r.loc.x + r.size.w - after_x, r.size.h)),
                    );
                    push_clipped(element.clone(), before);
                    push_clipped(element, after);
                }
            }
            // The focus fill, behind the names and in front of the bar: it slides to a
            // neighbour, and fades across when other names lie between.
            if let (true, Some((_, fills, _))) = (sliding, &highlight_rect) {
                for ((rect, alpha), cell) in fills
                    .iter()
                    .zip([&self.bar_highlight_fill, &self.bar_highlight_fade])
                {
                    if *alpha <= 0. {
                        continue;
                    }
                    let size = rect.size;
                    let geometry = Rectangle::from_size(size);
                    let mut element = cell.borrow_mut();
                    element.update(
                        size,
                        geometry,
                        Default::default(),
                        accent,
                        accent,
                        0.,
                        geometry,
                        0.,
                        CornerRadius::from((size.h / 2.) as f32)
                            .fit_to(size.w as f32, size.h as f32),
                        scale as f32,
                        *alpha,
                    );
                    let element = MonitorInnerRenderElement::Decoration(
                        element.clone().with_location(rect.loc).into(),
                    );
                    push(RelocateRenderElement::from_element(
                        RescaleRenderElement::from_element(element, Point::default(), 1.),
                        Point::default(),
                        Relocate::Relative,
                    ));
                }
            }
            // Put the shared surfaces behind the window targets.
            for (rect, label) in [
                (strip.tray, &self.strip_tray),
                (strip.floating_tray, &self.strip_floating_tray),
            ] {
                let Some(rect) = rect else { continue };
                let margin = Point::from((BAR_SHADOW_MARGIN, BAR_SHADOW_MARGIN));
                if let Some(tray) = label.render_with_ink(
                    renderer.as_gles_renderer(),
                    String::new(),
                    String::new(),
                    surface,
                    ink,
                    LabelKind::BarTray,
                    rect.size.w,
                    scale,
                    rect.loc - margin,
                    1.,
                ) {
                    let element = MonitorInnerRenderElement::Caption(tray);
                    push(RelocateRenderElement::from_element(
                        RescaleRenderElement::from_element(element, Point::default(), 1.),
                        Point::default(),
                        Relocate::Relative,
                    ));
                }
            }
            return;
        }
        let ws = self.active_workspace_ref();
        let accent = ws.options.layout.border.active_color;
        let surface = caption_background(accent, ws.options.layout.background_color, false);
        let color = tint_caption(surface, accent);
        let Some(bounds) = self.cue_bounds(ws) else {
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
            } else if !cue.armed {
                String::new()
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
                    if cue.armed {
                        LabelKind::Button { align_right: !left }
                    } else {
                        LabelKind::EdgeHandle {
                            height: cue.rect.size.h.round() as u32,
                        }
                    }
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

        // Elements pushed first draw in front: the menu covers everything else.
        self.render_layout_menu(ctx.renderer, push);
        self.render_folded_tabs(ctx.renderer, ctx.target, push);
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
                let mut crop_bounds = crop_bounds;
                if pass == 3 && ws.id() == self.active_workspace_ref().id() {
                    self.render_peek_scrims(ctx.renderer, ws, &mut |elem| push(elem));
                }
                let visible_x = (pass == 3)
                    .then(|| self.scrolling_visible_span(ws))
                    .flatten();
                let mut shadow_crop = crop_bounds;
                if let Some(bounds) = &visible_x {
                    let left = ((bounds.start - geo.loc.x) * scale).ceil() as i32;
                    let right = ((bounds.end - geo.loc.x) * scale).floor() as i32;
                    crop_bounds.loc.x = left;
                    crop_bounds.size.w = right.saturating_sub(left).max(0);
                    if let Some(top) = self.bar_row_top() {
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

                match pass {
                    0 => {
                        ws.render_floating(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::MovingBetweenWorkspaces,
                            push!(),
                        );
                    }
                    1 => {
                        ws.render_floating(
                            ctx.r(),
                            xray_pos,
                            focus_ring,
                            RenderLayer::Normal,
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
        self.active_workspace_ref()
            .options
            .layout
            .clip_scroll_to_work_area
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

    /// How far the overview presents workspaces as cards, from 0 (closed) to 1 (open).
    ///
    /// `None` outside the overview, or when `overview { workspace-cards; }` is off.
    pub fn workspace_cards_progress(&self) -> Option<f64> {
        if !self.options.overview.workspace_cards {
            return None;
        }
        self.overview_progress
            .as_ref()
            .map(|p| p.clamped_value().clamp(0., 1.))
    }

    /// Corner radius of the workspace cards, in workspace coordinates (before the overview
    /// zoom). It grows with the overview, so the desktop opens and closes square.
    pub fn workspace_card_radius(&self) -> f64 {
        self.workspace_cards_progress()
            .map_or(0., |p| WORKSPACE_CARD_RADIUS * p / self.overview_zoom())
    }

    /// Labels above the zoomed-out workspaces: a workspace's name, or "Workspace" and its
    /// number, marked "Empty" for the empty one at the end.
    pub fn render_workspace_labels<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let Some(progress) = self.workspace_cards_progress() else {
            return;
        };
        // Labels appear once the workspaces have shrunk enough to leave room above them.
        let alpha = ((progress - 0.4) / 0.6).clamp(0., 1.) as f32;
        if alpha <= 0. {
            return;
        }

        let _span = tracy_client::span!("Monitor::render_workspace_labels");

        let scale = self.scale.fractional_scale();
        let backdrop = self.options.overview.backdrop_color;
        let ink = if is_light(backdrop) {
            Color::new_unpremul(0.06, 0.13, 0.11, 1.)
        } else {
            Color::new_unpremul(0.95, 0.98, 0.96, 1.)
        };
        let gap = self.workspace_gap(self.overview_zoom());
        // Too little room between workspaces for a line of text.
        if gap < WORKSPACE_LABEL_HEIGHT + 4. {
            return;
        }

        let last = self.workspaces.len().saturating_sub(1);
        let mut labels = self.workspace_labels.borrow_mut();
        labels.resize_with(self.workspaces.len(), PreviewLabel::default);
        for ((idx, ws), geo) in self.workspaces_with_render_geo_idx() {
            // Numbered like the workspaces in a bar; the empty one at the end says so.
            let text = match ws.name() {
                Some(name) => name.clone(),
                None if idx == last && !ws.has_windows() => {
                    format!("Workspace {} · Empty", idx + 1)
                }
                None => format!("Workspace {}", idx + 1),
            };
            // The current workspace's name is bright, in the focus ring's colour
            // when it is on; the others recede.
            let current = idx == self.active_workspace_idx;
            let emphasis = if current { 1. } else { 0.68 };
            let ink = if current && !self.options.layout.focus_ring.off {
                let accent = self.options.layout.focus_ring.active_color;
                Color::new_unpremul(accent.r, accent.g, accent.b, 1.)
            } else {
                ink
            };
            // The caption pads its text; align the text, not the texture, with the card.
            let loc = Point::from((
                geo.loc.x - WORKSPACE_LABEL_INSET,
                geo.loc.y - WORKSPACE_LABEL_HEIGHT - (gap - WORKSPACE_LABEL_HEIGHT).min(12.) / 2.,
            ));
            let width = geo.size.w.min(420.) + WORKSPACE_LABEL_INSET;
            if let Some(label) = labels[idx].render_with_ink(
                renderer.as_gles_renderer(),
                text,
                String::new(),
                backdrop,
                ink,
                LabelKind::GroupTitle,
                width,
                scale,
                loc,
                alpha * emphasis,
            ) {
                push(RelocateRenderElement::from_element(
                    RescaleRenderElement::from_element(
                        MonitorInnerRenderElement::Caption(label),
                        Point::default(),
                        1.,
                    ),
                    Point::default(),
                    Relocate::Relative,
                ));
            }
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

impl<W: LayoutElement> Monitor<W> {
    /// The layout button of `window`'s titlebar, in output coordinates, while it is on screen.
    fn layout_button_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        if self.overview_progress.is_some() || self.workspace_switch.is_some() {
            return None;
        }
        let active = self.active_workspace_ref().id();
        let (ws, geo) = self
            .workspaces_with_render_geo()
            .find(|(ws, _)| ws.id() == active)?;
        let (tile, pos, visible) = ws
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == window)?;
        if !visible {
            return None;
        }
        let rect = tile.titlebar_button_rect(TitlebarPart::Layout)?;
        let rect = Rectangle::new(geo.loc + pos + rect.loc, rect.size);
        Rectangle::from_size(self.view_size)
            .intersection(rect)
            .filter(|visible| visible.size.w >= rect.size.w / 2.)
            .map(|_| rect)
    }

    fn layout_menu_geometry(&self) -> Option<LayoutMenuGeometry<W::Id>> {
        let menu = self.layout_menu.borrow();
        let menu = menu.as_ref()?;
        let anchor = self.layout_button_rect(&menu.window)?;
        let state = self
            .active_workspace_ref()
            .layout_menu_state(&menu.window)?;
        Some(LayoutMenuGeometry {
            window: menu.window.clone(),
            anchor,
            rect: layout_menu::place(anchor, self.working_area),
            state,
            hover: menu.hover,
            hovered: menu.hovered,
        })
    }

    /// Opens the layout menu of `window`. A menu opened by pointing (`hover`) closes again when
    /// the pointer leaves it.
    pub fn open_layout_menu(&self, window: &W::Id, hover: bool) -> bool {
        if self.layout_button_rect(window).is_none() {
            return false;
        }
        let mut menu = self.layout_menu.borrow_mut();
        if hover && menu.as_ref().is_some_and(|menu| menu.window == *window) {
            return false;
        }
        *menu = Some(OpenLayoutMenu {
            window: window.clone(),
            hover,
            hovered: None,
        });
        true
    }

    /// Clicking the layout button opens its menu, keeps open a menu that opened from pointing
    /// at it, or closes it.
    pub fn toggle_layout_menu(&self, window: &W::Id) -> bool {
        let open = self
            .layout_menu
            .borrow()
            .as_ref()
            .filter(|menu| menu.window == *window)
            .map(|menu| menu.hover);
        match open {
            Some(true) => {
                if let Some(menu) = self.layout_menu.borrow_mut().as_mut() {
                    menu.hover = false;
                }
                true
            }
            Some(false) => self.close_layout_menu(),
            None => self.open_layout_menu(window, false),
        }
    }

    pub fn close_layout_menu(&self) -> bool {
        self.layout_menu.borrow_mut().take().is_some()
    }

    /// The window whose layout menu is open.
    pub fn layout_menu_window(&self) -> Option<W::Id> {
        self.layout_menu
            .borrow()
            .as_ref()
            .map(|menu| menu.window.clone())
    }

    /// What a press at `position` reaches in the layout menu: `None` outside the menu, and the
    /// item under it, if any, inside.
    pub fn layout_menu_under(
        &self,
        position: Point<f64, Logical>,
    ) -> Option<Option<(W::Id, LayoutMenuItem)>> {
        let menu = self.layout_menu_geometry()?;
        if !menu.rect.contains(position) {
            return None;
        }
        Some(
            layout_menu::item_at(&menu.state, position - menu.rect.loc)
                .map(|item| (menu.window, item)),
        )
    }

    /// Whether `position` is on the layout button that opened the menu.
    pub fn layout_menu_anchor_contains(&self, position: Point<f64, Logical>) -> bool {
        self.layout_menu_geometry()
            .is_some_and(|menu| menu.anchor.contains(position))
    }

    /// Follows the pointer: highlights the item under it, and closes a menu that opened from
    /// pointing once the pointer leaves the button and the menu. Returns whether anything
    /// changed.
    pub fn update_layout_menu_pointer(&self, position: Option<Point<f64, Logical>>) -> bool {
        if self.layout_menu.borrow().is_none() {
            return false;
        }
        let Some(menu) = self.layout_menu_geometry() else {
            // The window closed, scrolled away or lost its titlebar.
            return self.close_layout_menu();
        };
        if menu.hover {
            // The span between the button and the menu keeps it open on the way across.
            let span = menu.anchor.merge(menu.rect);
            if position.is_none_or(|position| !span.contains(position)) {
                return self.close_layout_menu();
            }
        }
        let hovered = position
            .filter(|position| menu.rect.contains(*position))
            .and_then(|position| layout_menu::item_at(&menu.state, position - menu.rect.loc));
        if hovered == menu.hovered {
            return false;
        }
        if let Some(open) = self.layout_menu.borrow_mut().as_mut() {
            open.hovered = hovered;
        }
        true
    }

    fn render_layout_menu<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let Some(menu) = self.layout_menu_geometry() else {
            return;
        };
        let ws = self.active_workspace_ref();
        let titlebar = &ws.options.layout.titlebar;
        // Opaque, like the window bar.
        let background = Color {
            a: 1.,
            ..titlebar.background_color(false)
        };
        let ink = titlebar.text_color(true);
        let accent = ws.options.layout.border.active_color;
        let scale = self.scale.fractional_scale();
        let Some(element) = self.layout_menu_texture.render(
            renderer.as_gles_renderer(),
            menu.state,
            menu.hovered,
            background,
            ink,
            accent,
            scale,
            menu.rect.loc,
            1.,
        ) else {
            return;
        };
        let element = MonitorInnerRenderElement::Caption(element);
        push(RelocateRenderElement::from_element(
            RescaleRenderElement::from_element(element, Point::default(), 1.),
            Point::default(),
            Relocate::Relative,
        ));
    }
}

impl<W: LayoutElement> Monitor<W> {
    /// Tabs for the folded pinned columns of the active workspace, at the middle of their edges.
    /// Whether `position` is on one of the controls the compositor draws over the windows: the
    /// window bar, the edge arrows, the tabs of folded columns or the layout menu.
    fn overlay_contains(&self, position: Point<f64, Logical>) -> bool {
        if self.overview_progress.is_some() {
            return false;
        }
        if let Some(strip) = self.navigation_strip() {
            if [strip.tray, strip.floating_tray]
                .into_iter()
                .flatten()
                .any(|rect| rect.contains(position))
            {
                return true;
            }
        }
        if self
            .scroll_cues()
            .into_iter()
            .flatten()
            .any(|cue| cue.opacity > 0.5 && cue.rect.contains(position))
        {
            return true;
        }
        let hover = self.folded_hover.get();
        if self
            .folded_tabs()
            .iter()
            .any(|tab| tab.hit_area(hover).contains(position))
        {
            return true;
        }
        self.layout_menu_under(position).is_some()
    }

    /// The lines a floating window snaps to on the active workspace, horizontal then vertical,
    /// in output coordinates: the working area one gap in, its middle, and the edges of the
    /// other shown floating windows, aligned or one gap apart.
    pub(super) fn floating_snap_lines(&self, exclude: &W::Id) -> (SnapLines, SnapLines) {
        let ws = self.active_workspace_ref();
        let area = ws.floating().working_area();
        let gaps = ws.options.layout.gaps;
        let mut xs = SnapLines::default();
        let mut ys = SnapLines::default();
        xs.edges
            .extend([area.loc.x + gaps, area.loc.x + area.size.w - gaps]);
        ys.edges
            .extend([area.loc.y + gaps, area.loc.y + area.size.h - gaps]);
        xs.middles.push(area.loc.x + area.size.w / 2.);
        ys.middles.push(area.loc.y + area.size.h / 2.);
        if ws.is_floating_visible() {
            for (tile, pos) in ws.floating().tiles_with_render_positions() {
                if tile.is_hidden() || tile.window().id() == exclude {
                    continue;
                }
                let size = tile.tile_size();
                xs.edges
                    .extend([pos.x, pos.x + size.w, pos.x - gaps, pos.x + size.w + gaps]);
                ys.edges
                    .extend([pos.y, pos.y + size.h, pos.y - gaps, pos.y + size.h + gaps]);
            }
        }
        (xs, ys)
    }

    /// Whether a shown floating window of `ws` covers part of `rect`.
    fn covered_by_floating(&self, ws: &Workspace<W>, rect: Rectangle<f64, Logical>) -> bool {
        ws.is_floating_visible()
            && ws
                .floating()
                .tiles_with_render_positions()
                .filter(|(tile, _)| !tile.is_hidden())
                .any(|(tile, pos)| Rectangle::new(pos, tile.tile_size()).overlaps(rect))
    }

    fn folded_tabs(&self) -> Vec<FoldedTab<'_, W>> {
        if self.overview_progress.is_some() || self.workspace_switch.is_some() {
            return Vec::new();
        }
        let ws = self.active_workspace_ref();
        let area = self.working_area;
        let bounds = self
            .scroll_clip(ws)
            .unwrap_or(area.loc.x..area.loc.x + area.size.w);
        let gap = ws.options.layout.gaps.max(8.);
        let height = 72.;
        let y = area.loc.y + (area.size.h - height) / 2.;
        let name_width = 200.;
        ws.scrolling()
            .folded_pinned_columns()
            .map(|(side, window)| {
                let (gap_start, name_x) = match side {
                    PinSide::Left => (bounds.start, bounds.start + gap),
                    PinSide::Right => (bounds.end - gap, bounds.end - gap - name_width),
                };
                FoldedTab {
                    side,
                    window,
                    // The whole gap at the tab's height takes the pointer; the tab is drawn
                    // in its middle.
                    tab: Rectangle::new(Point::from((gap_start, y)), Size::from((gap, height))),
                    name: Rectangle::new(
                        Point::from((name_x, y + (height - SCROLL_BUTTON_HEIGHT) / 2.)),
                        Size::from((name_width, SCROLL_BUTTON_HEIGHT)),
                    ),
                }
            })
            // Shown floating windows cover the tabs, as they cover the tiled windows.
            .filter(|tab| !self.covered_by_floating(ws, tab.tab))
            .collect()
    }

    fn render_folded_tabs<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
        push: &mut dyn FnMut(MonitorRenderElement<R>),
    ) {
        let tabs = self.folded_tabs();
        if tabs.is_empty() {
            return;
        }
        let ws = self.active_workspace_ref();
        let accent = ws.options.layout.border.active_color;
        let titlebar = &ws.options.layout.titlebar;
        let surface = Color {
            a: 1.,
            ..titlebar.background_color(false)
        };
        let scale = self.scale.fractional_scale();
        let hover = self.folded_hover.get();
        let mut draw = |label| {
            let elem = MonitorInnerRenderElement::Caption(label);
            let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
            push(RelocateRenderElement::from_element(
                elem,
                Point::default(),
                Relocate::Relative,
            ));
        };
        for tab in tabs {
            let index = usize::from(tab.side == PinSide::Right);
            let hovered = hover == Some(tab.side);
            if hovered {
                let title = navigation_title(tab.window, target, "Pinned window");
                let text = match tab.side {
                    PinSide::Left => format!("‹  {title}"),
                    PinSide::Right => format!("{title}  ›"),
                };
                if let Some(label) = self.folded_name_labels[index].render(
                    renderer.as_gles_renderer(),
                    text,
                    accent,
                    LabelKind::Button {
                        align_right: tab.side == PinSide::Right,
                    },
                    tab.name.size.w,
                    scale,
                    tab.name.loc,
                    1.,
                ) {
                    draw(label);
                }
            }
            let drawn = tab.drawn_tab();
            if let Some(label) = self.folded_tab_labels[index].render(
                renderer.as_gles_renderer(),
                String::new(),
                if hovered { accent } else { surface },
                LabelKind::EdgeHandle {
                    height: drawn.size.h.round() as u32,
                },
                drawn.size.w,
                scale,
                drawn.loc,
                if hovered { 1. } else { 0.9 },
            ) {
                draw(label);
            }
        }
    }
}

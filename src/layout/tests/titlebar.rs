use super::*;
use niri_config::titlebar::{ButtonsSide, Titlebar as TitlebarConfig, TitlebarButton};

use crate::layout::titlebar::{TitlebarLayout, TitlebarPart};

fn hit(size: Size<f64, Logical>, point: Point<f64, Logical>) -> Option<TitlebarPart> {
    TitlebarLayout::new(&TitlebarConfig::default(), size, true).hit(point)
}

fn tile(ssd: bool, scale: f64, border: bool) -> Tile<TestWindow> {
    tile_with_rules(ssd, scale, border, None)
}

fn tile_with_rules(
    ssd: bool,
    scale: f64,
    border: bool,
    rules: Option<ResolvedWindowRules>,
) -> Tile<TestWindow> {
    let mut params = TestWindowParams::new(1);
    params.rules = rules;
    let window = TestWindow::new(params);
    window.0.has_ssd.set(ssd);
    let mut options = Options::default();
    options.animations.off = true;
    options.layout.titlebar.off = false;
    options.layout.border.off = !border;
    options.layout.border.width = 1.;
    let options = Rc::new(options);
    let mut clock = Clock::with_time(Duration::ZERO);
    clock.set_complete_instantly(true);
    let mut tile = Tile::new(window, (1280., 720.).into(), scale, clock, options.clone());
    tile.update_config((1280., 720.).into(), scale, options);
    tile
}

#[test]
fn only_negotiated_ssd_reserves_titlebar_space() {
    let mut tile = tile(false, 1., false);
    assert_eq!(tile.tile_size(), Size::from((100., 200.)));
    assert_eq!(tile.window_loc(), Point::from((0., 0.)));
    tile.window().0.has_ssd.set(true);
    tile.update_window();
    assert_eq!(tile.tile_size(), Size::from((100., 232.)));
    assert_eq!(tile.window_loc(), Point::from((0., 32.)));
    tile.window().0.has_ssd.set(false);
    tile.update_window();
    assert_eq!(tile.tile_size(), Size::from((100., 200.)));
}

#[test]
fn frame_size_and_content_offset_agree_at_fractional_scales() {
    for scale in [1., 1.25, 1.5, 2.] {
        let mut tile = tile(true, scale, true);
        tile.request_tile_size((602., 402.).into(), false, None);
        let expected = tile.window().0.requested_size.get().unwrap();
        let border = tile.effective_border_width().unwrap();
        assert_eq!(expected.h, (402. - border * 2. - 32.).floor() as i32);
        tile.window().communicate();
        tile.update_window();
        let location = tile.window_loc();
        assert!((location.x - border).abs() <= 1. / scale);
        assert!((location.y - border - 32.).abs() <= 1. / scale);
        assert_eq!(
            tile.window_height_for_tile_height(tile.tile_height_for_window_height(200.)),
            200.
        );
        assert!(matches!(
            tile.hit((border + 5., border + 5.).into()),
            Some(HitType::Titlebar { .. })
        ));
        assert!(!matches!(
            tile.hit((border + 5., location.y + 5.).into()),
            Some(HitType::Titlebar { .. })
        ));
    }
}

#[test]
fn maximized_keeps_titlebar_fullscreen_uses_the_whole_output() {
    let mut tile = tile(true, 1., false);
    tile.request_maximized((1280., 700.).into(), false, None);
    assert_eq!(
        tile.window().0.requested_size.get(),
        Some((1280, 668).into())
    );
    tile.window().communicate();
    tile.update_window();
    assert_eq!(tile.tile_size(), Size::from((1280., 700.)));
    tile.request_fullscreen(false, None);
    assert_eq!(
        tile.window().0.requested_size.get(),
        Some((1280, 720).into())
    );
    tile.window().communicate();
    tile.update_window();
    tile.advance_animations();
    assert_eq!(tile.tile_size(), Size::from((1280., 720.)));
    assert_eq!(tile.window_loc(), Point::from((0., 0.)));
}

#[test]
fn zero_maximum_stays_unbounded_and_disabled_titlebars_reserve_nothing() {
    let mut tile = tile(true, 1., false);
    assert_eq!(tile.max_size_nonfullscreen(), Size::from((0., 0.)));
    assert_eq!(tile.min_size_nonfullscreen().h, 32.);
    let mut options = (*tile.options).clone();
    options.layout.titlebar.off = true;
    tile.update_config((1280., 720.).into(), 1., Rc::new(options));
    assert_eq!(tile.titlebar_height(), 0.);
    assert_eq!(tile.tile_size(), Size::from((100., 200.)));
}

#[test]
fn hit_regions_are_disjoint_and_controls_remain_reachable_in_narrow_windows() {
    let size = Size::from((300., 32.));
    assert_eq!(hit(size, (299., 16.).into()), Some(TitlebarPart::Close));
    assert_eq!(hit(size, (250., 16.).into()), Some(TitlebarPart::Maximize));
    assert_eq!(hit(size, (10., 16.).into()), Some(TitlebarPart::Drag));
    assert_eq!(hit(size, (100., 16.).into()), Some(TitlebarPart::Drag));
    assert_eq!(hit(size, (300., 16.).into()), None);
    assert_eq!(hit(size, (100., 32.).into()), None);
    assert_eq!(
        hit((20., 32.).into(), (19., 16.).into()),
        Some(TitlebarPart::Close)
    );
    assert_eq!(
        hit((20., 32.).into(), (1., 16.).into()),
        Some(TitlebarPart::Maximize)
    );
}

#[test]
fn negotiating_ssd_during_a_resize_keeps_the_frame_inside_the_tile() {
    let mut tile = tile(false, 1., false);
    tile.clock.set_complete_instantly(false);
    tile.window().0.has_ssd.set(true);
    tile.request_tile_size((100., 200.).into(), true, None);
    tile.window().communicate();
    tile.update_window();
    // The previous frame had no titlebar. At the start of the resize it must still align
    // with its tile, rather than starting half a new titlebar above it.
    assert_eq!(tile.window_loc(), Point::from((0., 0.)));
    assert_eq!(tile.animated_tile_size(), Size::from((100., 200.)));
    tile.clock.set_complete_instantly(true);
    tile.advance_animations();
    assert_eq!(tile.window_loc(), Point::from((0., 32.)));
    assert_eq!(tile.tile_size(), Size::from((100., 200.)));
}

#[test]
fn button_order_side_and_fixed_size_windows() {
    let size = Size::from((300., 32.));
    let config = TitlebarConfig {
        buttons: vec![TitlebarButton::Close, TitlebarButton::Maximize],
        buttons_side: ButtonsSide::Left,
        ..Default::default()
    };
    let layout = TitlebarLayout::new(&config, size, true);
    assert_eq!(layout.hit((1., 16.).into()), Some(TitlebarPart::Close));
    assert_eq!(layout.hit((40., 16.).into()), Some(TitlebarPart::Maximize));
    assert_eq!(layout.hit((299., 16.).into()), Some(TitlebarPart::Drag));
    assert_eq!(layout.text.loc.x, 64. + 8.);
    assert_eq!(layout.text.size.w, 300. - 72. - 12.);

    // A window that cannot change size offers no maximize control.
    let fixed = TitlebarLayout::new(&TitlebarConfig::default(), size, false);
    assert_eq!(fixed.buttons.len(), 1);
    assert_eq!(fixed.hit((299., 16.).into()), Some(TitlebarPart::Close));
    assert_eq!(fixed.hit((250., 16.).into()), Some(TitlebarPart::Drag));

    let none = TitlebarConfig {
        buttons: Vec::new(),
        ..Default::default()
    };
    let none = TitlebarLayout::new(&none, size, true);
    assert_eq!(none.hit((299., 16.).into()), Some(TitlebarPart::Drag));
    assert_eq!(none.text.size.w, 300. - 24.);
}

#[test]
fn window_rule_overrides_the_layout_setting() {
    let rules = |draw| ResolvedWindowRules {
        draw_titlebar: Some(draw),
        ..Default::default()
    };
    let tile = tile_with_rules(true, 1., false, Some(rules(false)));
    assert_eq!(tile.titlebar_height(), 0.);

    let mut tile = tile_with_rules(true, 1., false, Some(rules(true)));
    let mut options = (*tile.options).clone();
    options.layout.titlebar.off = true;
    tile.update_config((1280., 720.).into(), 1., Rc::new(options));
    assert_eq!(tile.titlebar_height(), 32.);
}

#[test]
fn size_requests_follow_the_pending_decoration_mode() {
    let mut tile = tile(true, 1., false);
    tile.window().0.pending_ssd.set(Some(false));
    tile.request_tile_size((100., 400.).into(), false, None);
    assert_eq!(tile.window().0.requested_size.get().unwrap().h, 400);
    // The committed frame still has its titlebar until the client commits the new mode.
    assert_eq!(tile.titlebar_height(), 32.);
}

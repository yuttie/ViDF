//! Bevy front end: draws pages and overlays and feeds keys to the editor.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use bevy::asset::RenderAssetUsages;
use bevy::input::keyboard::{Key as BevyKey, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::sprite::Anchor;
use bevy::text::FontSize;
use bevy::window::PrimaryWindow;
use bevy::winit::{EventLoopProxyWrapper, WinitSettings, WinitUserEvent};

use crate::editor::{Editor, Effect, Key, Shape};
use crate::worker::{self, Request, Response, Waker, Worker};

const STATUS_HEIGHT: f32 = 24.0;
const STATUS_FONT: f32 = 14.0;
/// Largest rendered page side in pixels; bounds memory at high zoom.
const MAX_TEXTURE: f32 = 6144.0;
const BACKGROUND: Color = Color::srgb(0.18, 0.18, 0.2);

#[derive(Resource, Deref, DerefMut, Default)]
struct Ed(Editor);

#[derive(Resource)]
struct Backend {
    worker: Worker,
    waker: Waker,
    opened: bool,
}

#[derive(Component)]
struct PageSprite {
    page: usize,
    shown: Option<f32>,
    requested: Option<f32>,
}

#[derive(Component)]
struct Overlay;

#[derive(Component)]
struct StatusLeft;

#[derive(Component)]
struct StatusRight;

/// What the overlay was last built for.
#[derive(Resource, Default)]
struct Drawn {
    generation: u64,
    zoom: f32,
}

pub fn run(path: PathBuf) -> AppExit {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let waker = Waker::default();
    let worker = worker::spawn(path, waker.clone());
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window { title: format!("{name} - ViDF"), ..default() }),
            ..default()
        }))
        .insert_resource(WinitSettings::desktop_app())
        .insert_resource(ClearColor(BACKGROUND))
        .insert_resource(Backend { worker, waker, opened: false })
        .init_resource::<Ed>()
        .init_resource::<Drawn>()
        .add_systems(Startup, setup)
        .add_systems(Update, (poll_worker, keyboard, sync_view, request_renders, draw_overlay, draw_status).chain());
    #[cfg(feature = "e2e")]
    e2e::install(&mut app);
    app.run()
}

fn setup(mut commands: Commands, backend: Res<Backend>, proxy: Res<EventLoopProxyWrapper>) {
    let proxy = Mutex::new((**proxy).clone());
    let wake = move || {
        let _ = proxy.lock().unwrap().send_event(WinitUserEvent::WakeUp);
    };
    // Responses sent before the waker was installed still need a frame.
    wake();
    let _ = backend.waker.set(Box::new(wake));

    commands.spawn(Camera2d);
    let font = TextFont::from_font_size(FontSize::Px(STATUS_FONT));
    let color = TextColor(Color::srgb(0.9, 0.9, 0.9));
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(0.0),
            left: Val::Px(0.0),
            width: Val::Percent(100.0),
            height: Val::Px(STATUS_HEIGHT),
            padding: UiRect::horizontal(Val::Px(8.0)),
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(Color::srgb(0.1, 0.1, 0.12)),
        children![(Text::new(""), font.clone(), color, StatusLeft), (Text::new(""), font, color, StatusRight)],
    ));
}

fn viewport_size(window: &Window) -> Vec2 {
    Vec2::new(window.width(), (window.height() - STATUS_HEIGHT).max(1.0))
}

fn poll_worker(
    mut commands: Commands,
    mut backend: ResMut<Backend>,
    mut ed: ResMut<Ed>,
    mut images: ResMut<Assets<Image>>,
    mut sprites: Query<(&mut PageSprite, &mut Sprite)>,
    window: Single<&Window, With<PrimaryWindow>>,
    mut exit: MessageWriter<AppExit>,
) {
    let responses: Vec<Response> = backend.worker.rx.lock().unwrap().try_iter().collect();
    for resp in responses {
        match resp {
            Response::Opened { pages, annots, warning } => {
                ed.open(pages, annots, viewport_size(&window));
                for page in 0..ed.view.pages.len() {
                    commands.spawn((
                        PageSprite { page, shown: None, requested: None },
                        Sprite::from_color(Color::WHITE, Vec2::ONE),
                        Transform::default(),
                    ));
                }
                if let Some(w) = warning {
                    ed.message = w;
                }
                backend.opened = true;
            }
            Response::Text { page, text } => ed.set_text(page, text),
            Response::Rendered { page, scale, width, height, rgba } => {
                let Some((mut ps, mut sprite)) = sprites.iter_mut().find(|(ps, _)| ps.page == page) else { continue };
                if ps.requested.is_none() {
                    // Cancelled while rendering; the page is no longer near the view.
                    continue;
                }
                if ps.requested == Some(scale) {
                    ps.requested = None;
                }
                let size = Extent3d { width, height, depth_or_array_layers: 1 };
                let image = Image::new(
                    size,
                    TextureDimension::D2,
                    rgba,
                    TextureFormat::Rgba8UnormSrgb,
                    RenderAssetUsages::RENDER_WORLD,
                );
                sprite.image = images.add(image);
                ps.shown = Some(scale);
            }
            Response::Saved(Ok(n)) => {
                ed.saved(n);
                if ed.quit_after_save {
                    exit.write(AppExit::Success);
                }
            }
            Response::Saved(Err(e)) => {
                ed.quit_after_save = false;
                ed.message = format!("write failed: {e}");
            }
            Response::Failed(e) if !backend.opened => {
                eprintln!("vidf: {e}");
                exit.write(AppExit::error());
            }
            Response::Failed(e) => ed.message = e,
        }
    }
}

fn convert_key(ev: &KeyboardInput, ctrl: bool) -> Option<Key> {
    Some(match &ev.logical_key {
        BevyKey::Character(s) => {
            let c = s.chars().next()?;
            if ctrl { Key::Ctrl(c.to_ascii_lowercase()) } else { Key::Char(c) }
        }
        BevyKey::Space => Key::Char(' '),
        BevyKey::Escape => Key::Esc,
        BevyKey::Enter => Key::Enter,
        BevyKey::Backspace => Key::Backspace,
        BevyKey::ArrowUp => Key::Up,
        BevyKey::ArrowDown => Key::Down,
        BevyKey::ArrowLeft => Key::Left,
        BevyKey::ArrowRight => Key::Right,
        BevyKey::PageUp => Key::PageUp,
        BevyKey::PageDown => Key::PageDown,
        _ => return None,
    })
}

fn yank(text: String) -> Result<(), String> {
    let mut child = std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("wl-copy: {e}"))?;
    child.stdin.take().unwrap().write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

fn keyboard(
    mut events: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    mut ed: ResMut<Ed>,
    backend: Res<Backend>,
    mut exit: MessageWriter<AppExit>,
) {
    if !backend.opened {
        events.clear();
        return;
    }
    let ctrl = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]);
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        let Some(k) = convert_key(ev, ctrl) else { continue };
        press(k, &mut ed, &backend, &mut exit);
    }
}

fn press(k: Key, ed: &mut Editor, backend: &Backend, exit: &mut MessageWriter<AppExit>) {
    for fx in ed.key(k) {
        match fx {
            Effect::Save => {
                let annots = ed.annots.clone();
                let _ = backend.worker.tx.send(Request::Save { annots });
            }
            Effect::Quit => {
                exit.write(AppExit::Success);
            }
            Effect::Yank(text) => {
                if let Err(e) = yank(text) {
                    ed.message = e;
                }
            }
        }
    }
}

fn sync_view(
    mut ed: ResMut<Ed>,
    window: Single<&Window, With<PrimaryWindow>>,
    mut camera: Single<&mut Transform, With<Camera2d>>,
    mut pages: Query<(&PageSprite, &mut Sprite, &mut Transform), Without<Camera2d>>,
) {
    if ed.view.is_empty() {
        return;
    }
    let size = viewport_size(&window);
    if ed.view.size != size {
        ed.view.size = size;
        ed.view.scroll(Vec2::ZERO);
    }
    // The viewport sits above the status bar.
    let c = ed.view.center - Vec2::new(0.0, STATUS_HEIGHT / 2.0);
    if camera.translation.truncate() != c {
        camera.translation = c.extend(camera.translation.z);
    }
    for (ps, mut sprite, mut tf) in &mut pages {
        let r = ed.view.page_rect(ps.page);
        if sprite.custom_size != Some(r.size()) {
            sprite.custom_size = Some(r.size());
        }
        let t = r.center().extend(0.0);
        if tf.translation != t {
            tf.translation = t;
        }
    }
}

fn request_renders(
    ed: Res<Ed>,
    backend: Res<Backend>,
    window: Single<&Window, With<PrimaryWindow>>,
    mut pages: Query<(&mut PageSprite, &mut Sprite)>,
) {
    let view = &ed.view;
    if view.is_empty() {
        return;
    }
    let vp = view.viewport();
    // Render one screen ahead, and free textures more than two screens away.
    let near = Rect::from_center_size(vp.center(), vp.size() * Vec2::new(1.0, 3.0));
    let far = Rect::from_center_size(vp.center(), vp.size() * Vec2::new(1.0, 5.0));
    let dpr = window.scale_factor();
    for (mut ps, mut sprite) in &mut pages {
        let r = view.page_rect(ps.page);
        let rect_y = |a: Rect| r.max.y >= a.min.y && r.min.y <= a.max.y;
        if rect_y(near) {
            let info = &view.pages[ps.page];
            let scale = (view.zoom * dpr).min(MAX_TEXTURE / info.size.max_element());
            let close = |s: Option<f32>| s.is_some_and(|s| (s / scale - 1.0).abs() < 0.01);
            if !close(ps.shown) && !close(ps.requested) {
                let _ = backend.worker.tx.send(Request::Render { page: ps.page, scale });
                ps.requested = Some(scale);
            }
        } else if !rect_y(far) {
            if ps.shown.take().is_some() {
                sprite.image = Handle::default();
            }
            if ps.requested.take().is_some() {
                let _ = backend.worker.tx.send(Request::Cancel { page: ps.page });
            }
        }
    }
}

fn srgba(c: [f32; 4]) -> Color {
    Color::srgba(c[0], c[1], c[2], c[3])
}

fn draw_overlay(
    mut commands: Commands,
    ed: Res<Ed>,
    mut drawn: ResMut<Drawn>,
    old: Query<Entity, With<Overlay>>,
) {
    if drawn.generation == ed.generation && drawn.zoom == ed.view.zoom {
        return;
    }
    drawn.generation = ed.generation;
    drawn.zoom = ed.view.zoom;
    for e in &old {
        commands.entity(e).despawn();
    }
    let view = &ed.view;
    if view.is_empty() {
        return;
    }
    for shape in ed.shapes() {
        match shape {
            Shape::Quad { page, quad, color, z } => {
                let [ul, ur, ll, lr] = [quad.ul, quad.ur, quad.ll, quad.lr].map(|p| view.to_world(page, p));
                let center = (ul + ur + ll + lr) / 4.0;
                let along = ur - ul;
                let size = Vec2::new(along.length(), (ll - ul).length());
                commands.spawn((
                    Overlay,
                    Sprite::from_color(srgba(color), size),
                    Transform::from_translation(center.extend(z)).with_rotation(Quat::from_rotation_z(along.to_angle())),
                ));
            }
            Shape::Rect { page, rect, color, z } => {
                let r = view.rect_to_world(page, rect);
                commands.spawn((
                    Overlay,
                    Sprite::from_color(srgba(color), r.size()),
                    Transform::from_translation(r.center().extend(z)),
                ));
            }
            Shape::Text { page, at, text, size, color, z } => {
                commands.spawn((
                    Overlay,
                    Text2d::new(text),
                    TextFont::from_font_size(FontSize::Px(size * view.zoom)),
                    TextColor(Color::srgb(color[0], color[1], color[2])),
                    Anchor::TOP_LEFT,
                    Transform::from_translation(view.to_world(page, at).extend(z)),
                ));
            }
        }
    }
}

fn draw_status(
    ed: Res<Ed>,
    mut left: Single<&mut Text, (With<StatusLeft>, Without<StatusRight>)>,
    mut right: Single<&mut Text, (With<StatusRight>, Without<StatusLeft>)>,
) {
    if !ed.is_changed() {
        return;
    }
    let (l, r) = ed.status();
    if left.0 != l {
        left.0 = l;
    }
    if right.0 != r {
        right.0 = r;
    }
}

/// Scripted runs for end-to-end checks. Once the document is loaded, the
/// keys in `VIDF_KEYS` are pressed (Vim notation such as `<Esc>`, `<CR>`,
/// `<C-d>`), then the window is captured to `VIDF_SHOT` and the app exits.
#[cfg(feature = "e2e")]
mod e2e {
    use bevy::render::view::screenshot::{save_to_disk, Screenshot};

    use super::*;

    #[derive(Resource)]
    struct Script {
        keys: Vec<Key>,
        shot: PathBuf,
        frames: u32,
        stage: u8,
    }

    fn parse(s: &str) -> Vec<Key> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            if c == '<' {
                if let Some(end) = rest.find('>') {
                    let k = match &rest[1..end] {
                        "Esc" => Some(Key::Esc),
                        "CR" => Some(Key::Enter),
                        "BS" => Some(Key::Backspace),
                        "Space" => Some(Key::Char(' ')),
                        n if n.starts_with("C-") && n.len() == 3 => n.chars().nth(2).map(Key::Ctrl),
                        _ => None,
                    };
                    if let Some(k) = k {
                        out.push(k);
                        rest = &rest[end + 1..];
                        continue;
                    }
                }
            }
            out.push(Key::Char(c));
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    pub fn install(app: &mut App) {
        let (Ok(keys), Ok(shot)) = (std::env::var("VIDF_KEYS"), std::env::var("VIDF_SHOT")) else { return };
        app.insert_resource(WinitSettings::game())
            .insert_resource(Script { keys: parse(&keys), shot: shot.into(), frames: 0, stage: 0 })
            .add_systems(Update, step.after(draw_status));
    }

    fn step(
        mut commands: Commands,
        mut script: ResMut<Script>,
        mut ed: ResMut<Ed>,
        backend: Res<Backend>,
        pages: Query<&PageSprite>,
        mut exit: MessageWriter<AppExit>,
    ) {
        let settled = backend.opened
            && ed.text.pages.iter().all(Option::is_some)
            && pages.iter().all(|p| p.requested.is_none());
        script.frames += 1;
        match script.stage {
            0 if settled => {
                for k in std::mem::take(&mut script.keys) {
                    press(k, &mut ed, &backend, &mut exit);
                }
                script.stage = 1;
                script.frames = 0;
            }
            1 if settled && script.frames > 5 => {
                commands.spawn(Screenshot::primary_window()).observe(save_to_disk(script.shot.clone()));
                script.stage = 2;
                script.frames = 0;
            }
            2 if script.frames > 10 => {
                exit.write(AppExit::Success);
            }
            _ => {}
        }
    }
}

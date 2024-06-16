use std::{error::Error, fs::File, os::unix::io::AsFd, process::ExitCode};

use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_subcompositor, wl_subsurface, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use image::{io::Reader as ImageReader, Pixel};
use memmap2::MmapMut;
use rand::Rng;

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;

    let mut event_queue = conn.new_event_queue();
    let qhandle = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qhandle, ());

    let mut state = State::new()?;
    event_queue.blocking_dispatch(&mut state)?;

    state.registry_post_process(&qhandle);

    while state.running {
        event_queue.roundtrip(&mut state)?;

        std::thread::sleep(std::time::Duration::from_millis(40));
        state.draw();
    }

    Ok(ExitCode::SUCCESS)
}

struct State {
    running: bool,
    configured: bool,
    fullscreen_requested: bool,

    compositor: Option<wl_compositor::WlCompositor>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,

    parent_surface: Option<wl_surface::WlSurface>,
    parent_xdg_surface: Option<(xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel)>,
    parent_buffer: Option<wl_buffer::WlBuffer>,

    child_surface: Option<wl_surface::WlSurface>,
    child_subsurface: Option<wl_subsurface::WlSubsurface>,
    child_buffer: Option<wl_buffer::WlBuffer>,

    file: File,
    mmap: MmapMut,
    buffer_pool_size: u64,

    animation: Animation,
}

impl State {
    fn new() -> Result<State, Box<dyn Error>> {
        let mut rng = rand::thread_rng();
        let side = rand::distributions::Uniform::new(2, 30);

        let animation = Animation::new(rng.sample(side), 15, 6);

        let buffer_pool_size = (animation.frame().len() + 4) as _;
        let file = tempfile::tempfile()?;
        file.set_len(buffer_pool_size)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(State {
            running: true,
            configured: false,
            fullscreen_requested: false,

            compositor: None,
            subcompositor: None,
            shm: None,
            wm_base: None,

            parent_surface: None,
            parent_xdg_surface: None,
            parent_buffer: None,

            child_surface: None,
            child_subsurface: None,
            child_buffer: None,

            file,
            mmap,
            buffer_pool_size,

            animation,
        })
    }

    fn registry_post_process(&mut self, qh: &QueueHandle<Self>) {
        let compositor = self.compositor.as_ref().unwrap();
        let parent_surface = compositor.create_surface(qh, ());
        let child_surface = compositor.create_surface(qh, ());

        let wm_base = self.wm_base.as_ref().unwrap();
        let parent_xdg_surface = wm_base.get_xdg_surface(&parent_surface, qh, ());
        let toplevel = parent_xdg_surface.get_toplevel(qh, ());
        toplevel.set_title("Gopher on Wayland".into());
        toplevel.set_fullscreen(None);
        parent_surface.commit();
        self.fullscreen_requested = true;

        let subcompositor = self.subcompositor.as_ref().unwrap();
        let child_subsurface =
            subcompositor.get_subsurface(&child_surface, &parent_surface, qh, ());
        child_subsurface.set_sync();

        let frame = &self.animation.frame();
        let shm = self.shm.as_ref().unwrap();
        let pool = shm.create_pool(self.file.as_fd(), self.buffer_pool_size as i32, qh, ());

        let (init_w, init_h) = frame.dimensions();
        self.child_buffer = Some(pool.create_buffer(
            0,
            init_w as i32,
            init_h as i32,
            (init_w * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        ));

        let (init_w, init_h) = (1, 1);
        self.parent_buffer = Some(pool.create_buffer(
            frame.len() as _,
            init_w,
            init_h,
            init_w * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        ));
        parent_surface.attach(self.parent_buffer.as_ref(), 0, 0);

        self.parent_surface = Some(parent_surface);
        self.parent_xdg_surface = Some((parent_xdg_surface, toplevel));
        self.child_surface = Some(child_surface);
        self.child_subsurface = Some(child_subsurface);
    }

    fn draw(&mut self) {
        if !self.configured {
            return;
        }

        let frame = &self.animation.frame();

        let mut i = 0;
        for pixel in frame.pixels() {
            let p = pixel.channels();
            self.mmap[i..i + 4].copy_from_slice(&[p[2], p[1], p[0], p[3]]);
            i += 4;
        }

        let child_subsurface = self.child_subsurface.as_ref().unwrap();
        let position = self.animation.position();
        child_subsurface.set_position(position.0, position.1);

        let child_surface = self.child_surface.as_ref().unwrap();
        child_surface.attach(self.child_buffer.as_ref(), 0, 0);
        child_surface.damage(0, 0, frame.width() as i32, frame.height() as i32);
        child_surface.commit();

        let parent_surface = self.parent_surface.as_ref().unwrap();
        parent_surface.commit();

        self.animation.next();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match &interface[..] {
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, 1, qh, ()));
                }
                "wl_subcompositor" => {
                    state.subcompositor = Some(
                        registry.bind::<wl_subcompositor::WlSubcompositor, _, _>(name, 1, qh, ()),
                    );
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }
                "wl_seat" => {
                    registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, ());
                }
                "xdg_wm_base" => {
                    state.wm_base =
                        Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

// Ignore events from these object types in this example.
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial, .. } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                //dbg!(width, height, &states);

                if states.contains(&(xdg_toplevel::State::Fullscreen as _))
                    && state.fullscreen_requested
                {
                    state.fullscreen_requested = false;
                    state.animation.area = (width as _, height as _);

                    state.draw();
                }
            }
            xdg_toplevel::Event::Close => state.running = false,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key { key, .. } = event {
            if key == 1 {
                // ESC key
                state.running = false;
            }
        }
    }
}

enum JumpState {
    NotJumping,
    Ascending(u64),
    Descending(u64),
}

impl JumpState {
    fn next(&mut self, jump_step: u64, jump_count: u64) {
        let limit = jump_step * jump_count;
        *self = match *self {
            JumpState::Ascending(y) if y >= limit => JumpState::Descending(limit - jump_step),
            JumpState::Ascending(y) if y >= (limit as f64 * 0.6) as u64 => {
                JumpState::Ascending(y + jump_step / 4)
            }
            JumpState::Ascending(y) => JumpState::Ascending(y + jump_step),
            JumpState::Descending(0) => JumpState::NotJumping,
            JumpState::Descending(y) if y >= (limit as f64 * 0.6) as u64 => {
                JumpState::Descending(y.saturating_sub(jump_step / 4))
            }
            JumpState::Descending(y) => JumpState::Descending(y.saturating_sub(jump_step)),
            JumpState::NotJumping => JumpState::NotJumping,
        };
    }
}

struct Animation {
    x: u64,
    y: u64,
    area: (u64, u64),
    count: u64,
    jump: JumpState,
    forward: bool,

    walk_step: u64,
    jump_count: u64,
    jump_step: u64,

    frames: Vec<image::RgbaImage>,
    frames_flipped: Vec<image::RgbaImage>,
    frame_index: usize,
}

impl Animation {
    fn new(walk_step: u64, jump_step: u64, jump_count: u64) -> Self {
        const IMAGE_PATHS: [&str; 3] = ["image/out01.png", "image/out02.png", "image/out03.png"];

        let frames: Vec<_> = IMAGE_PATHS
            .iter()
            .filter_map(|path| ImageReader::open(path).ok())
            .filter_map(|reader| reader.decode().ok())
            .map(|img| img.into_rgba8())
            .collect();

        let frames_flipped = frames
            .iter()
            .map(image::imageops::flip_horizontal)
            .collect();

        Self {
            x: 0,
            y: 0,
            area: (0, 0),
            count: 0,
            jump: JumpState::NotJumping,
            forward: true,

            walk_step,
            jump_count,
            jump_step,

            frames,
            frames_flipped,
            frame_index: 0,
        }
    }

    fn position(&self) -> (i32, i32) {
        (
            self.x as _,
            (self.area.1 - (self.frame().height() as u64) - self.y) as _,
        )
    }

    fn frame(&self) -> &image::RgbaImage {
        if self.forward {
            &self.frames[self.frame_index]
        } else {
            &self.frames_flipped[self.frame_index]
        }
    }

    fn next(&mut self) {
        //dbg!(self.x, self.y, self.position(), self.area);

        self.count += 1;
        self.jump.next(self.jump_step, self.jump_count);

        let walk_step = match self.jump {
            JumpState::Ascending(y) | JumpState::Descending(y) => {
                self.y = y;
                self.frame_index = 0;
                self.walk_step / 2
            }
            JumpState::NotJumping => {
                self.frame_index = if self.frame_index == 2 {
                    0
                } else {
                    self.frame_index + 1
                };

                if self.count % 45 == 0 {
                    self.jump = JumpState::Ascending(0);
                }

                self.walk_step
            }
        };

        if self.forward {
            self.x += walk_step;
            if self.x >= (self.area.0 - self.frame().width() as u64) {
                self.forward = false;
                self.x = self.area.0 - self.frame().width() as u64;
            }
        } else {
            self.x = self.x.saturating_sub(walk_step);
            self.forward = self.x == 0;
        }
    }
}

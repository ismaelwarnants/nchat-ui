//! Headless Slint platform for screenshots/smoke tests: renders with the software renderer
//! into memory, no display needed. Enabled when NCHAT_UI_SNAPSHOT is set.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel};
use slint::platform::{EventLoopProxy, Platform, WindowAdapter};
use slint::{EventLoopError, PhysicalSize};

pub const WIDTH: u32 = 1180;
pub const HEIGHT: u32 = 760;

type Task = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    tasks: Mutex<Vec<Task>>,
    quit: AtomicBool,
}

struct Proxy(Arc<Queue>);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        self.0.quit.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn invoke_from_event_loop(&self, event: Box<dyn FnOnce() + Send>) -> Result<(), EventLoopError> {
        self.0.tasks.lock().unwrap().push(event);
        Ok(())
    }
}

pub struct Headless {
    window: Rc<MinimalSoftwareWindow>,
    queue: Arc<Queue>,
}

thread_local! {
    static WINDOW: std::cell::RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { std::cell::RefCell::new(None) };
}

pub fn install() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
    WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
    slint::platform::set_platform(Box::new(Headless { window, queue: Arc::new(Queue::default()) }))
        .expect("set headless platform");
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Rgba(pub [u8; 4]);

impl TargetPixel for Rgba {
    fn blend(&mut self, c: PremultipliedRgbaColor) {
        let a = 255 - c.alpha as u16;
        self.0[0] = (c.red as u16 + (self.0[0] as u16 * a) / 255) as u8;
        self.0[1] = (c.green as u16 + (self.0[1] as u16 * a) / 255) as u8;
        self.0[2] = (c.blue as u16 + (self.0[2] as u16 * a) / 255) as u8;
        self.0[3] = 255;
    }
    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Rgba([r, g, b, 255])
    }
}

/// Renders the current window content into an RGBA buffer.
pub fn render() -> Option<Vec<u8>> {
    WINDOW.with(|w| {
        let window = w.borrow().clone()?;
        let mut buf = vec![Rgba::default(); (WIDTH * HEIGHT) as usize];
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buf, WIDTH as usize);
        });
        Some(buf.into_iter().flat_map(|p| p.0).collect())
    })
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(self.queue.clone())))
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        // Render every frame like a real window would, so lazy layout (ListView) happens.
        let mut scratch = vec![Rgba::default(); (WIDTH * HEIGHT) as usize];
        while !self.queue.quit.load(Ordering::SeqCst) {
            self.window.draw_if_needed(|renderer| {
                renderer.render(&mut scratch, WIDTH as usize);
            });
            let tasks: Vec<Task> = std::mem::take(&mut *self.queue.tasks.lock().unwrap());
            for t in tasks {
                t();
            }
            slint::platform::update_timers_and_animations();
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

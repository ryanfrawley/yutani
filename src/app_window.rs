use winit::{
    event_loop::EventLoopBuilder,
    event_loop::EventLoopProxy,
};


#[derive(Debug, Clone)]
pub enum CustomEvent {
    PtyInput(String),
}

pub struct AppWindow {
    pub event_loop_proxy: EventLoopProxy::<CustomEvent>,
}

impl AppWindow {
    pub fn new() -> Self {
        let event_loop = EventLoopBuilder::<CustomEvent>::with_user_event().build().unwrap();
        let event_loop_proxy = event_loop.create_proxy();

        Self {
            event_loop_proxy,
        }
    }
}

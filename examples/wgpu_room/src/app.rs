use crate::events::UiCmd;
use crate::logo_track::LogoTrack;
use crate::sine_track::SineTrack;
use crate::video_renderer::VideoRenderer;
use crate::{events::AsyncCmd, video_grid::VideoGrid};
use egui::{Rounding, Stroke};
use egui_wgpu::WgpuConfiguration;
use futures::StreamExt;
use livekit::prelude::*;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::SimulateScenario;
use parking_lot::deadlock;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{mpsc, oneshot};

// Useful default constants for developing
const DEFAULT_URL: &str = "ws://localhost:7880";
const DEFAULT_TOKEN : &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJleHAiOjE5MDY2MTMyODgsImlzcyI6IkFQSVRzRWZpZFpqclFvWSIsIm5hbWUiOiJuYXRpdmUiLCJuYmYiOjE2NzI2MTMyODgsInN1YiI6Im5hdGl2ZSIsInZpZGVvIjp7InJvb20iOiJ0ZXN0Iiwicm9vbUFkbWluIjp0cnVlLCJyb29tQ3JlYXRlIjp0cnVlLCJyb29tSm9pbiI6dHJ1ZSwicm9vbUxpc3QiOnRydWV9fQ.uSNIangMRu8jZD5mnRYoCHjcsQWCrJXgHCs0aNIgBFY";

use winit::{
    event::*,
    event_loop::{ControlFlow, EventLoop},
    window::{WindowBuilder, WindowId},
};

struct SessionHandle {
    room: Arc<Room>,
    logo_track: Arc<AsyncMutex<LogoTrack>>,
    sine_track: Arc<AsyncMutex<SineTrack>>,
    close_tx: oneshot::Sender<()>,
    task_handle: tokio::task::JoinHandle<()>,
}

struct AppState {
    session: Mutex<Option<SessionHandle>>,
    connecting: AtomicBool,
}

struct App {
    state: Arc<AppState>,
    video_renderers: HashMap<(ParticipantSid, TrackSid), VideoRenderer>,

    egui_context: egui::Context,
    egui_state: egui_winit::State,
    egui_painter: egui_wgpu::winit::Painter,
    window: winit::window::Window,
    cmd_tx: mpsc::UnboundedSender<AsyncCmd>,
    cmd_rx: mpsc::UnboundedReceiver<UiCmd>,

    // Ui State
    lk_url: String,
    lk_token: String,
    connection_failure: Option<String>,
    room_state: ConnectionState,

    auto_subscribe: bool,

    // Log events
    events: Vec<String>,
}

pub fn run(rt: tokio::runtime::Runtime) {
    rt.block_on(async {
        let event_loop = EventLoop::new();
        let window = WindowBuilder::new()
            .with_title("LiveKit - NativeSDK")
            .build(&event_loop)
            .unwrap();

        let egui_context = egui::Context::default();
        let egui_state = egui_winit::State::new(&event_loop);
        let mut egui_painter =
            egui_wgpu::winit::Painter::new(WgpuConfiguration::default(), 1, None, false);

        egui_painter.set_window(Some(&window)).await.unwrap();

        let (async_cmd_tx, mut async_cmd_rx) = mpsc::unbounded_channel::<AsyncCmd>();
        let (ui_cmd_tx, ui_cmd_rx) = mpsc::unbounded_channel::<UiCmd>();

        let state = Arc::new(AppState {
            session: Default::default(),
            connecting: AtomicBool::new(false),
        });

        let mut app = App {
            state: state.clone(),
            video_renderers: HashMap::default(),
            egui_context,
            egui_state,
            egui_painter,
            window,
            cmd_tx: async_cmd_tx,
            cmd_rx: ui_cmd_rx,
            lk_url: DEFAULT_URL.to_owned(),
            lk_token: DEFAULT_TOKEN.to_owned(),
            connection_failure: None,
            room_state: ConnectionState::Unknown,
            auto_subscribe: true,
            events: Vec::new(),
        };

        // Create a background thread which checks for deadlocks every 10s
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(10));
            let deadlocks = deadlock::check_deadlock();
            if deadlocks.is_empty() {
                continue;
            }

            log::error!("{} deadlocks detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                log::error!("Deadlock #{}", i);
                for t in threads {
                    log::error!("Thread Id {:#?}: \n{:#?}", t.thread_id(), t.backtrace());
                }
            }
        });

        // Async event loop
        tokio::spawn(async move {
            while let Some(event) = async_cmd_rx.recv().await {
                match event {
                    AsyncCmd::RoomConnect { url, token } => {
                        state.connecting.store(true, Ordering::SeqCst);

                        let res = Room::connect(&url, &token, RoomOptions::default()).await;
                        if let Ok((room, room_events)) = res {
                            let room = Arc::new(room);
                            let (close_tx, close_rx) = oneshot::channel();
                            let logo_track = LogoTrack::new(room.clone());
                            let sine_track = SineTrack::new(room.clone());
                            let task_handle = tokio::spawn(room_task(
                                state.clone(),
                                room_events,
                                close_rx,
                                ui_cmd_tx.clone(),
                            ));

                            *state.session.lock() = Some(SessionHandle {
                                room,
                                logo_track: Arc::new(AsyncMutex::new(logo_track)),
                                sine_track: Arc::new(AsyncMutex::new(sine_track)),
                                close_tx,
                                task_handle,
                            });

                            let _ = ui_cmd_tx.send(UiCmd::ConnectResult { result: Ok(()) });
                        } else if let Err(err) = res {
                            let _ = ui_cmd_tx.send(UiCmd::ConnectResult { result: Err(err) });
                        }

                        state.connecting.store(false, Ordering::SeqCst);
                    }
                    AsyncCmd::RoomDisconnect => {
                        let handle = state.session.lock().take();
                        if let Some(handle) = handle {
                            let _ = handle.room.close().await;
                            let _ = handle.close_tx.send(());
                            let _ = handle.task_handle.await;
                        }
                    }
                    AsyncCmd::SimulateScenario { scenario } => {
                        let room = {
                            state
                                .session
                                .lock()
                                .as_ref()
                                .map(|handle| handle.room.clone())
                        };

                        if let Some(room) = room {
                            let _ = room.simulate_scenario(scenario).await;
                        }
                    }
                    AsyncCmd::ToggleLogo => {
                        let logo_track = {
                            state
                                .session
                                .lock()
                                .as_ref()
                                .map(|handle| handle.logo_track.clone())
                        };

                        if let Some(logo_track) = logo_track {
                            let mut logo_track = logo_track.lock().await;
                            if !logo_track.is_published() {
                                logo_track.publish().await.unwrap();
                            } else {
                                logo_track.unpublish().await.unwrap();
                            }
                        }
                    }
                    AsyncCmd::ToggleSine => {
                        let sine_track = {
                            state
                                .session
                                .lock()
                                .as_ref()
                                .map(|handle| handle.sine_track.clone())
                        };

                        if let Some(sine_track) = sine_track {
                            let mut sine_track = sine_track.lock().await;
                            if !sine_track.is_published() {
                                sine_track.publish().await.unwrap();
                            } else {
                                sine_track.unpublish().await.unwrap();
                            }
                        }
                    }
                }
            }
        });

        tokio::task::block_in_place(move || loop {
            // ui/main thread
            event_loop.run(move |event, _, control_flow| {
                app.update(event, control_flow);
            });
        });
    });
}

async fn room_task(
    _app_state: Arc<AppState>,
    mut room_events: mpsc::UnboundedReceiver<RoomEvent>,
    mut close_rx: oneshot::Receiver<()>,
    ui_cmd_tx: mpsc::UnboundedSender<UiCmd>,
) {
    loop {
        tokio::select! {
            Some(event) = room_events.recv() => {
                let _ = ui_cmd_tx.send(UiCmd::RoomEvent{event});
            }
            _ = &mut close_rx => {
                break;
            }
        }
    }
}

impl App {
    fn update<T>(&mut self, event: Event<'_, T>, control_flow: &mut ControlFlow) {
        if let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                UiCmd::ConnectResult { result } => {
                    if let Err(err) = result {
                        self.connection_failure = Some(err.to_string());
                    } else {
                        self.connection_failure = None
                    }
                }
                UiCmd::RoomEvent { event } => {
                    self.events.push(format!("{:?}", event));

                    match event {
                        RoomEvent::TrackSubscribed {
                            track, participant, ..
                        } => {
                            match track.clone() {
                                RemoteTrack::Video(video_track) => {
                                    // Create a new VideoRenderer
                                    let video_renderer = VideoRenderer::new(
                                        self.egui_painter.render_state().clone().unwrap(),
                                        video_track.rtc_track(),
                                    );
                                    self.video_renderers
                                        .insert((participant.sid(), track.sid()), video_renderer);
                                }
                                RemoteTrack::Audio(audio_track) => {
                                    tokio::spawn(async move {
                                        let mut stream =
                                            NativeAudioStream::new(audio_track.rtc_track());

                                        while let Some(_frame) = stream.next().await {
                                            // TODO(theomonnom): Play audio using the libwebrtc ADM playout devices
                                        }
                                    });
                                }
                            };
                        }
                        RoomEvent::TrackUnsubscribed {
                            track, participant, ..
                        } => {
                            self.video_renderers
                                .remove(&(participant.sid(), track.sid()));
                        }
                        _ => {}
                    }
                }
            }
        }

        match event {
            Event::WindowEvent { window_id, event } => {
                if let Some(flow) = self.on_window_event(window_id, event) {
                    *control_flow = flow;
                }
            }
            Event::RedrawRequested(window_id) if window_id == self.window.id() => {
                self.render();
            }
            Event::RedrawEventsCleared => {
                self.window.request_redraw();
            }
            _ => {}
        };
    }

    fn on_window_event(
        &mut self,
        _window_id: WindowId,
        event: WindowEvent<'_>,
    ) -> Option<ControlFlow> {
        if self
            .egui_state
            .on_event(&self.egui_context, &event)
            .consumed
        {
            return None;
        }

        match event {
            WindowEvent::CloseRequested => Some(ControlFlow::Exit),
            WindowEvent::Resized(inner_size) => {
                self.egui_painter
                    .on_window_resized(inner_size.width, inner_size.height);
                None
            }
            WindowEvent::ScaleFactorChanged { new_inner_size, .. } => {
                self.egui_painter
                    .on_window_resized(new_inner_size.width, new_inner_size.height);
                None
            }
            _ => None,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let connecting = self.state.connecting.load(Ordering::SeqCst);
        let connected = self.state.session.lock().is_some();

        egui::TopBottomPanel::top("top_panel").show(ui.ctx(), |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button(
                    "Tools",
                    |ui| if ui.button("WebRTC Stats (TODO)").clicked() {},
                );
                ui.menu_button("Simulate", |ui| {
                    if ui.button("SignalReconnect").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::SimulateScenario {
                            scenario: SimulateScenario::SignalReconnect,
                        });
                    }
                    if ui.button("Speaker").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::SimulateScenario {
                            scenario: SimulateScenario::Speaker,
                        });
                    }
                    if ui.button("NodeFailure").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::SimulateScenario {
                            scenario: SimulateScenario::NodeFailure,
                        });
                    }
                    if ui.button("ServerLeave").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::SimulateScenario {
                            scenario: SimulateScenario::ServerLeave,
                        });
                    }
                    if ui.button("Migration").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::SimulateScenario {
                            scenario: SimulateScenario::Migration,
                        });
                    }
                    if ui.button("ForceTcp").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::SimulateScenario {
                            scenario: SimulateScenario::ForceTcp,
                        });
                    }
                    if ui.button("ForceTls").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::SimulateScenario {
                            scenario: SimulateScenario::ForceTls,
                        });
                    }
                });

                ui.menu_button("Publish", |ui| {
                    if ui.button("Logo").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::ToggleLogo);
                    }
                    if ui.button("SineWave").clicked() {
                        let _ = self.cmd_tx.send(AsyncCmd::ToggleSine);
                    }
                });
            });
        });

        egui::SidePanel::left("left_panel")
            .resizable(true)
            .default_width(256.0)
            .show(ui.ctx(), |ui| {
                ui.add_space(8.0);
                ui.monospace("Livekit - Connect to a room");
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label("URL: ");
                    ui.text_edit_singleline(&mut self.lk_url);
                });

                ui.horizontal(|ui| {
                    ui.label("Token: ");
                    ui.text_edit_singleline(&mut self.lk_token);
                });

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!connected && !connecting, |ui| {
                        if ui.button("Connect").clicked() {
                            self.connection_failure = None;
                            let _ = self.cmd_tx.send(AsyncCmd::RoomConnect {
                                url: self.lk_url.clone(),
                                token: self.lk_token.clone(),
                            });
                        }
                    });

                    if connecting {
                        ui.spinner();
                    }

                    if connected {
                        if ui.button("Disconnect").clicked() {
                            let _ = self.cmd_tx.send(AsyncCmd::RoomDisconnect);
                        }
                    }
                });

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!connected && !connecting, |ui| {
                        ui.checkbox(&mut self.auto_subscribe, "Auto Subscribe");
                    });
                });

                if let Some(err) = &self.connection_failure {
                    ui.colored_label(egui::Color32::RED, err);
                }

                ui.separator();

                {
                    // Room Info
                    if let Some(handle) = self.state.session.lock().as_ref() {
                        ui.label(format!("Name: {}", handle.room.name()));
                        ui.label(format!("SID: {}", handle.room.sid()));
                        ui.label(format!(
                            "ConnectionState: {:?}",
                            handle.room.connection_state()
                        ));
                        ui.label(format!(
                            "ParticipantCount: {:?}",
                            handle.room.participants().len() + 1
                        ));
                    }
                }
            });

        egui::SidePanel::right("right_panel")
            .resizable(true)
            .default_width(256.0)
            .show(ui.ctx(), |ui| {
                ui.monospace("Tracks");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Show all tracks
                    if let Some(session) = self.state.session.lock().as_ref() {
                        for (_, participant) in session.room.participants() {
                            for (_, track) in participant.tracks() {
                                let TrackPublication::Remote(track) = track else {
                                    unreachable!();
                                };

                                ui.group(|ui| {
                                    ui.label(format!("{} {}", track.sid().0, track.name()));
                                    ui.horizontal(|ui| {
                                        ui.label(format!("{:?}", track.source()));

                                        if track.is_subscribed() {
                                            ui.colored_label(egui::Color32::GREEN, "Subscribed");
                                        } else {
                                            ui.colored_label(egui::Color32::RED, "Unsubscribed");
                                        }

                                        if track.is_muted() {
                                            ui.colored_label(egui::Color32::DARK_GRAY, "Muted");
                                        }
                                    });
                                });
                            }
                        }
                    }
                });
            });

        egui::TopBottomPanel::bottom("bottom_panel")
            .resizable(true)
            .min_height(32.0)
            .show(ui.ctx(), |ui| {
                ui.monospace("Events");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for event in &self.events {
                        ui.label(event);
                    }
                });
            });

        egui::CentralPanel::default().show(ui.ctx(), |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                VideoGrid::new("default_grid")
                    .max_columns(6)
                    .show(ui, |ui| {
                        if self.room_state == ConnectionState::Disconnected {
                            for _ in 0..20 {
                                ui.video_frame(|ui| {
                                    egui::Frame::none().fill(egui::Color32::DARK_GRAY).show(
                                        ui,
                                        |ui| {
                                            ui.allocate_space(ui.available_size());
                                        },
                                    );
                                });
                            }
                        } else {
                            // Render participant videos
                            for ((participant_sid, _), video_renderer) in &self.video_renderers {
                                ui.video_frame(|ui| {
                                    let rect = ui.available_rect_before_wrap();
                                    ui.painter().rect(
                                        rect,
                                        Rounding::none(),
                                        egui::Color32::DARK_GRAY,
                                        Stroke::NONE,
                                    );

                                    if let Some(tex) = video_renderer.texture_id() {
                                        ui.painter().image(
                                            tex,
                                            rect,
                                            egui::Rect::from_min_max(
                                                egui::pos2(0.0, 0.0),
                                                egui::pos2(1.0, 1.0),
                                            ),
                                            egui::Color32::WHITE,
                                        );
                                    }

                                    let name =
                                        self.state.session.lock().as_ref().and_then(|handle| {
                                            handle
                                                .room
                                                .participants()
                                                .get(participant_sid)
                                                .map(|p| p.name())
                                        });

                                    if let Some(name) = name {
                                        ui.painter().text(
                                            egui::pos2(rect.min.x + 5.0, rect.max.y - 5.0),
                                            egui::Align2::LEFT_BOTTOM,
                                            name,
                                            egui::FontId::default(),
                                            egui::Color32::WHITE,
                                        );
                                    }
                                });
                            }
                        }
                    });
            });
        });
    }

    fn render(&mut self) {
        self.egui_state
            .set_pixels_per_point(egui_winit::native_pixels_per_point(&self.window));

        let raw_inputs = self.egui_state.take_egui_input(&self.window);
        let full_output = self.egui_context.clone().run(raw_inputs, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                self.ui(ui);
            });
        });
        let clipped_primitives = self.egui_context.tessellate(full_output.shapes);

        self.egui_painter.paint_and_update_textures(
            egui_winit::native_pixels_per_point(&self.window),
            [0.0, 0.0, 0.0, 0.0],
            &clipped_primitives,
            &full_output.textures_delta,
            false,
        );

        self.egui_state.handle_platform_output(
            &self.window,
            &self.egui_context,
            full_output.platform_output,
        );
    }
}

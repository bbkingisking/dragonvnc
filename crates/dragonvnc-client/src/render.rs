//! The actual client window: a `winit`+`wgpu` surface that displays
//! decoded video frames and turns window input events into
//! `dragonvnc_proto::InputEvent`s sent back to the server. Cross-platform
//! per the original design (Vulkan/Metal via wgpu, one binary for both
//! platforms) — nothing here is macOS-specific; only the decoder feeding
//! it frames (`videotoolbox` vs `vaapi`/software) differs per platform.

use std::sync::Arc;

use bytes::Bytes;
use dragonvnc_capture::PixelFormat;
use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use dragonvnc_proto::{InputEvent, PointerButton};

use crate::keymap;

const SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(positions[idx], 0.0, 1.0);
    out.uv = uvs[idx];
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

/// Sent from the network task to the render/event-loop thread whenever a
/// frame is decoded. Carries the actual pixel format/stride rather than
/// assuming one — see `dragonvnc_codec::DecodedFrame`'s doc for why that
/// matters (VideoToolbox's real output is BGRA, not RGBA).
pub enum RenderEvent {
    Frame { width: u32, height: u32, format: PixelFormat, stride: u32, pixels: Bytes },
    /// The network side gave up (lost connection, decode error, etc.) —
    /// shown so the window doesn't just silently freeze with no
    /// explanation, then the app exits.
    NetworkError(String),
}

fn pixel_format_to_wgpu(format: PixelFormat) -> Option<wgpu::TextureFormat> {
    match format {
        PixelFormat::Rgba | PixelFormat::Rgbx => Some(wgpu::TextureFormat::Rgba8Unorm),
        PixelFormat::Bgra | PixelFormat::Bgrx => Some(wgpu::TextureFormat::Bgra8Unorm),
    }
}

struct VideoTexture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

struct GraphicsState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    window: Arc<Window>,
    pipeline_rgba: wgpu::RenderPipeline,
    pipeline_bgra: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    video: Option<VideoTexture>,
}

pub struct App {
    gfx: Option<GraphicsState>,
    input_tx: UnboundedSender<InputEvent>,
    remote_size: Option<(u32, u32)>,
    last_error: Option<String>,
}

impl App {
    pub fn new(input_tx: UnboundedSender<InputEvent>) -> Self {
        Self { gfx: None, input_tx, remote_size: None, last_error: None }
    }

    fn send_input(&self, event: InputEvent) {
        // Receiver gone just means the network task already exited (e.g.
        // connection lost) — the window is about to close too, nothing to
        // do about a send that can no longer go anywhere.
        let _ = self.input_tx.send(event);
    }

    /// Scales a window-physical cursor position to the remote display's
    /// own coordinate space (they're rarely the same size). Independent
    /// per-axis scaling — a "stretch to fit", not letterboxed; good enough
    /// for v1, revisit if aspect-ratio mismatches turn out to matter.
    fn to_remote_coords(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let (remote_w, remote_h) = self.remote_size?;
        let gfx = self.gfx.as_ref()?;
        let size = gfx.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return None;
        }
        Some((
            (x / size.width as f64 * remote_w as f64) as f32,
            (y / size.height as f64 * remote_h as f64) as f32,
        ))
    }
}

fn make_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    surface_format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: "vs_main",
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: "fs_main",
            targets: &[Some(surface_format.into())],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

impl ApplicationHandler<RenderEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("dragonvnc")
                        .with_inner_size(winit::dpi::LogicalSize::new(1024, 640)),
                )
                .expect("failed to create window"),
        );
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).expect("failed to create wgpu surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("failed to find a wgpu adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
            .expect("failed to get wgpu device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps.formats.iter().copied().find(|f| f.is_srgb() == false).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        // One pipeline per surface... actually per *video* format: the
        // shader/pipeline itself doesn't depend on the video texture's
        // format (that's baked into the bind group's texture view, which
        // wgpu handles generically for any Float-sampled format) — only
        // the *surface's* format matters for the fragment output target.
        // So a single pipeline pair isn't actually needed per video
        // format; kept as two names for clarity that both RGBA- and
        // BGRA-sourced videos render through the same pipeline.
        let pipeline_rgba = make_pipeline(&device, &pipeline_layout, &shader, surface_format);
        let pipeline_bgra = make_pipeline(&device, &pipeline_layout, &shader, surface_format);

        self.gfx = Some(GraphicsState {
            surface,
            device,
            queue,
            config,
            window,
            pipeline_rgba,
            pipeline_bgra,
            bind_group_layout,
            sampler,
            video: None,
        });
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: RenderEvent) {
        match event {
            RenderEvent::Frame { width, height, format, stride, pixels } => {
                self.remote_size = Some((width, height));
                let Some(gfx) = &mut self.gfx else { return };
                let Some(wgpu_format) = pixel_format_to_wgpu(format) else {
                    tracing::warn!(?format, "no wgpu mapping for this pixel format, dropping frame");
                    return;
                };
                let needs_new = match &gfx.video {
                    Some(v) => v.width != width || v.height != height || v.format != wgpu_format,
                    None => true,
                };
                if needs_new {
                    let texture = gfx.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("video-frame"),
                        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu_format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let bind_group = gfx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: None,
                        layout: &gfx.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&gfx.sampler) },
                        ],
                    });
                    gfx.video = Some(VideoTexture { texture, bind_group, width, height, format: wgpu_format });
                }
                let video = gfx.video.as_ref().unwrap();
                gfx.queue.write_texture(
                    wgpu::ImageCopyTexture {
                        texture: &video.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &pixels,
                    wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(height) },
                    wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                );
                gfx.window.request_redraw();
            }
            RenderEvent::NetworkError(e) => {
                tracing::error!(error = %e, "network task ended");
                if let Some(gfx) = &self.gfx {
                    // The window itself is the only thing left to tell a
                    // person anything — without this, "the network task
                    // died" and "the app just froze" look identical from
                    // in front of the screen. This won't catch every case
                    // (a hard crash of the whole process shows nothing
                    // either), but a live window whose network side quietly
                    // gave up now says so instead of just going stale.
                    gfx.window.set_title(&format!("dragonvnc — disconnected: {e}"));
                    gfx.window.request_redraw();
                }
                self.last_error = Some(e);
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gfx) = &mut self.gfx {
                    gfx.config.width = size.width.max(1);
                    gfx.config.height = size.height.max(1);
                    gfx.surface.configure(&gfx.device, &gfx.config);
                }
            }
            WindowEvent::RedrawRequested => {
                let Some(gfx) = &mut self.gfx else { return };
                let frame = match gfx.surface.get_current_texture() {
                    Ok(frame) => frame,
                    // This used to be `let Ok(frame) = ... else { return }`
                    // — silently dropping the redraw on *any* error,
                    // including `Lost`/`Outdated` (e.g. after a resize, a
                    // Spaces switch, or the window being occluded then
                    // un-occluded on macOS). Once the surface is in that
                    // state it stays broken until reconfigured, so every
                    // future frame's `request_redraw` would silently no-op
                    // forever — the window visibly frozen on its last good
                    // frame while video keeps decoding in the background.
                    // That's a real, distinct "freeze" bug, not just a
                    // missing log line: reconfigure and try again.
                    Err(e @ (wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated)) => {
                        tracing::warn!(error = %e, "wgpu surface lost/outdated, reconfiguring");
                        gfx.surface.configure(&gfx.device, &gfx.config);
                        gfx.window.request_redraw();
                        return;
                    }
                    Err(e) => {
                        // `Timeout` is common and harmless (e.g. the window
                        // is occluded/minimized) — not worth more than
                        // debug. Anything else (`OutOfMemory`, `Other`) is
                        // unexpected; surface loudly.
                        if matches!(e, wgpu::SurfaceError::Timeout) {
                            tracing::debug!(error = %e, "surface acquire timed out, skipping this redraw");
                        } else {
                            tracing::warn!(error = %e, "surface acquire failed, skipping this redraw");
                        }
                        return;
                    }
                };
                let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder = gfx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    if let Some(video) = &gfx.video {
                        let pipeline = match video.format {
                            wgpu::TextureFormat::Bgra8Unorm => &gfx.pipeline_bgra,
                            _ => &gfx.pipeline_rgba,
                        };
                        pass.set_pipeline(pipeline);
                        pass.set_bind_group(0, &video.bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                }
                let present_started = std::time::Instant::now();
                gfx.queue.submit(Some(encoder.finish()));
                frame.present();
                let present_elapsed = present_started.elapsed();
                if present_elapsed > std::time::Duration::from_millis(100) {
                    // `Fifo` present mode blocks for vsync, so some wait is
                    // normal — but this long means something's actually
                    // stuck (GPU driver, compositor, or the window fully
                    // occluded/minimized long enough to matter), not just
                    // "waiting one frame".
                    tracing::warn!(?present_elapsed, "submit+present took unusually long");
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((x, y)) = self.to_remote_coords(position.x, position.y) {
                    self.send_input(InputEvent::PointerMove { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => PointerButton::Left,
                    MouseButton::Right => PointerButton::Right,
                    MouseButton::Middle => PointerButton::Middle,
                    _ => return, // no back/forward/other-button mapping yet
                };
                self.send_input(InputEvent::PointerButton { button, pressed: state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    // Lines aren't pixels; this constant is a rough,
                    // untuned approximation of "one wheel click" — revisit
                    // once there's a real feel to tune against.
                    MouseScrollDelta::LineDelta(x, y) => (x * 40.0, y * 40.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                self.send_input(InputEvent::Scroll { dx, dy });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if let Some(keycode) = keymap::to_evdev(code) {
                        self.send_input(InputEvent::Key { keycode, pressed: event.state == ElementState::Pressed });
                    }
                }
            }
            _ => {}
        }
    }
}

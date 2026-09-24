//! Headless repro of the path rasterization pipeline
//! (`draw_paths_to_intermediate` + `draw_paths_from_intermediate`) for
//! diagnosing the GitGraph lane aliasing on near-horizontal segments.
//!
//! It rasterizes the exact lane geometry used by
//! `lane_tessellation_width` in `gpui::path_builder` (vertical run ->
//! quadratic bend -> horizontal run, 1.5px stroke) through the same WGSL
//! entry points, intermediate texture, MSAA, and blit pass that the
//! production renderer uses, then reports per-column / per-row coverage
//! profiles for the vertical run, the horizontal run, and the bend.

use super::*;
use gpui::{ContentMask, Hsla, PathBuilder, ScaledPixels, point, px, solid_background};
use std::num::NonZeroU64;

/// The exact GitGraph "checkout curve" geometry from
/// `lane_tessellation_width`: vertical run -> quadratic bend -> horizontal
/// run, stroked at 1.5px.
fn lane_path(width: u32, height: u32) -> Path<ScaledPixels> {
    let x0 = 100.0;
    let y_top = 50.0;
    let to_row_y = 200.0;
    let curve_h = 8.0; // row_height / 3
    let curve_w = 16.0 / 3.0; // LANE_WIDTH / 3
    let p_start = (x0, to_row_y - curve_h);
    let p_end = (x0 + curve_w, to_row_y);
    let p_ctrl = (x0, to_row_y);

    let mut builder = PathBuilder::stroke(px(1.5));
    builder.move_to(point(px(x0), px(y_top)));
    builder.line_to(point(px(p_start.0), px(p_start.1)));
    builder.move_to(point(px(p_start.0), px(p_start.1)));
    builder.curve_to(
        point(px(p_end.0), px(p_end.1)),
        point(px(p_ctrl.0), px(p_ctrl.1)),
    );
    builder.move_to(point(px(p_end.0), px(p_end.1)));
    builder.line_to(point(px(p_end.0 + 50.0), px(to_row_y)));
    let mut path = builder.build().unwrap();

    // `Window::paint_path` gives every path the top-of-stack content mask
    // (the full viewport when the panel is unclipped). Without it,
    // `clipped_bounds` would be empty and nothing would be rasterized.
    path.content_mask = ContentMask {
        bounds: Bounds {
            origin: Point::default(),
            size: Size {
                width: px(width as f32),
                height: px(height as f32),
            },
        },
    };
    // Solid opaque white, so the alpha channel is pure coverage.
    path.color = solid_background(Hsla {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a: 1.0,
    });
    path.scale(1.0)
}

fn readback(
    instance: &wgpu::Instance,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let size = (width as u64) * (height as u64) * 4;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("repro_readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(wgpu::MapMode::Read, 0..size, move |r| {
        let _ = tx.send(r);
    });
    // Headless: no event loop pumps the instance, so poll to deliver the
    // map callback and wait for the copy to complete.
    instance.poll_all(true);
    let _ = rx.recv().expect("map_async callback did not run");
    let data: Vec<u8> = buffer.get_mapped_range(0..size).to_vec();
    buffer.unmap();
    data
}

/// Rasterizes `path` exactly like the production renderer (same entry
/// points, blending, MSAA + resolve, and blit pass) and returns the
/// readback of the resolved intermediate and of the final post-blit image.
fn rasterize(
    instance: &wgpu::Instance,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    sample_count: u32,
    path: &Path<ScaledPixels>,
) -> (Vec<u8>, Vec<u8>) {
    // --- 1. Rasterization vertices, as in `draw_paths_to_intermediate`. ---
    let bounds = path.clipped_bounds();
    let vertices: Vec<PathRasterizationVertex> = path
        .vertices
        .iter()
        .map(|v| PathRasterizationVertex {
            xy_position: v.xy_position,
            st_position: v.st_position,
            color: path.color,
            bounds,
        })
        .collect();
    let vertices_size =
        (vertices.len() as u64 * std::mem::size_of::<PathRasterizationVertex>() as u64).max(16);

    let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("repro_instance_buffer"),
        size: vertices_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&instance_buffer, 0, unsafe {
        WgpuRenderer::instance_bytes(&vertices)
    });

    // --- 2. Globals, with the same offsets as `WgpuRenderer::new`. ---
    let uniform_alignment = device.limits().min_uniform_buffer_offset_alignment as u64;
    let globals_size = std::mem::size_of::<GlobalParams>() as u64;
    let gamma_size = std::mem::size_of::<GammaParams>() as u64;
    let path_globals_offset = globals_size.next_multiple_of(uniform_alignment);
    let gamma_offset = (path_globals_offset + globals_size).next_multiple_of(uniform_alignment);
    let globals = GlobalParams {
        viewport_size: [width as f32, height as f32],
        premultiplied_alpha: 1,
        pad: 0,
    };
    let gamma = GammaParams::zeroed();
    let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("repro_globals_buffer"),
        size: gamma_offset + gamma_size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&globals_buffer, 0, bytemuck::bytes_of(&globals));
    queue.write_buffer(
        &globals_buffer,
        path_globals_offset,
        bytemuck::bytes_of(&globals),
    );
    queue.write_buffer(&globals_buffer, gamma_offset, bytemuck::bytes_of(&gamma));

    let layouts = WgpuRenderer::create_bind_group_layouts(device, false);
    let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("repro_globals_bind_group"),
        layout: &layouts.globals,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &globals_buffer,
                    offset: path_globals_offset,
                    size: Some(NonZeroU64::new(globals_size).unwrap()),
                }),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &globals_buffer,
                    offset: gamma_offset,
                    size: Some(NonZeroU64::new(gamma_size).unwrap()),
                }),
            },
        ],
    });
    let instances_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("repro_instances_bind_group"),
        layout: &layouts.instances,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: &instance_buffer,
                offset: 0,
                size: Some(NonZeroU64::new(vertices_size).unwrap()),
            }),
        }],
    });

    // --- 3. Textures, as in `create_path_intermediate`/`create_msaa_if_needed`. ---
    let intermediate = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("repro_path_intermediate"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let intermediate_view = intermediate.create_view(&wgpu::TextureViewDescriptor::default());

    let msaa_view = if sample_count > 1 {
        Some(
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("repro_path_msaa"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default()),
        )
    } else {
        None
    };

    let final_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("repro_final"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let final_view = final_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // --- 4. Pipelines, as in `create_pipeline` in `WgpuRenderer::new`. ---
    let shader_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("repro_shaders"),
        source: wgpu::ShaderSource::Wgsl(STORAGE_BUFFER_SHADERS.into()),
    });

    let raster_layout = {
        let bgl = vec![Some(&layouts.globals), Some(&layouts.instances)];
        device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("repro_raster_layout"),
            bind_group_layouts: &bgl,
            immediate_size: 0,
        })
    };
    // Same specialization as `WgpuRenderer::create_pipelines`: the analytic
    // stroke coverage is disabled for MSAA pipelines, so each sample count
    // exercises its own code path.
    let stroke_aa = [(
        "STROKE_ANALYTIC_AA",
        if sample_count == 1 { 1.0 } else { 0.0 },
    )];
    let stroke_aa_options = wgpu::PipelineCompilationOptions {
        constants: &stroke_aa,
        ..Default::default()
    };
    let path_rasterization = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("repro_path_rasterization"),
        layout: Some(&raster_layout),
        vertex: wgpu::VertexState {
            module: &shader_module,
            entry_point: Some("vs_path_rasterization"),
            buffers: &[],
            compilation_options: stroke_aa_options.clone(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader_module,
            entry_point: Some("fs_path_rasterization"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: stroke_aa_options,
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: sample_count,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview_mask: None,
        cache: None,
    });

    let blit_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("repro_blit_sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("repro_blit_bind_group"),
        layout: &layouts.texture,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&intermediate_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&blit_sampler),
            },
        ],
    });

    let paths_blend = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    };
    let paths_layout = {
        let bgl = vec![
            Some(&layouts.globals),
            Some(&layouts.instances),
            Some(&layouts.texture),
        ];
        device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("repro_paths_layout"),
            bind_group_layouts: &bgl,
            immediate_size: 0,
        })
    };
    let paths = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("repro_paths"),
        layout: Some(&paths_layout),
        vertex: wgpu::VertexState {
            module: &shader_module,
            entry_point: Some("vs_path"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader_module,
            entry_point: Some("fs_path"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(paths_blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview_mask: None,
        cache: None,
    });

    // --- 5. Rasterization pass, as in `draw_paths_to_intermediate`. ---
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("repro_path_rasterization_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: msaa_view.as_ref().unwrap_or(&intermediate_view),
                resolve_target: msaa_view.as_ref().map(|_| &intermediate_view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        pass.set_pipeline(&path_rasterization);
        pass.set_bind_group(0, &globals_bind_group, &[]);
        pass.set_bind_group(1, &instances_bind_group, &[]);
        pass.draw(0..vertices.len() as u32, 0..1);
    }

    // --- 6. Blit pass, as in `draw_paths_from_intermediate`. ---
    {
        let sprite = PathSprite { bounds };
        let sprites = [sprite];
        let sprites_size = (std::mem::size_of::<PathSprite>() as u64).max(16);
        let sprite_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("repro_sprite_buffer"),
            size: sprites_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&sprite_buffer, 0, unsafe {
            WgpuRenderer::instance_bytes(&sprites)
        });
        let sprites_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("repro_sprites_bind_group"),
            layout: &layouts.instances,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &sprite_buffer,
                    offset: 0,
                    size: Some(NonZeroU64::new(sprites_size).unwrap()),
                }),
            }],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("repro_blit_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &final_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        pass.set_pipeline(&paths);
        pass.set_bind_group(0, &globals_bind_group, &[]);
        pass.set_bind_group(1, &sprites_bind_group, &[]);
        pass.set_bind_group(2, &blit_bind_group, &[]);
        pass.draw(0..4, 0..1);
    }

    queue.submit([encoder.finish()]);

    let data1 = readback(instance, device, queue, &intermediate, width, height);
    let data2 = readback(instance, device, queue, &final_texture, width, height);
    (data1, data2)
}

/// Per-region coverage profiles for the lane geometry.
fn report(sample_count: u32, label: &str, bytes: &[u8], width: u32, height: u32) {
    assert_eq!(bytes.len(), (width * height * 4) as usize);
    let alpha = |x: u32, y: u32| -> f32 {
        let i = (y as usize * width as usize + x as usize) * 4;
        bytes[i + 3] as f32 / 255.0
    };
    let join = |xs: &[f32]| {
        xs.iter()
            .map(|v| format!("{v:.3}"))
            .collect::<Vec<_>>()
            .join(" ")
    };

    // Vertical run: stroke centered at x=100 over y 50..192; the rows 55..187
    // skip both stroke ends.
    let vert_cols: Vec<f32> = (97..=103)
        .map(|x| (55..=187).map(|y| alpha(x, y)).sum())
        .collect();
    let vert_width: f32 = vert_cols.iter().sum::<f32>() / 133.0;

    // Horizontal run: stroke centered at y=200 over x 105.33..155.33; the
    // columns 115..145 skip both stroke ends.
    let horiz_rows: Vec<f32> = (198..=202)
        .map(|y| (115..=145).map(|x| alpha(x, y)).sum())
        .collect();
    let horiz_width: f32 = horiz_rows.iter().sum::<f32>() / 31.0;

    // Bend: the quadratic and both joins, x 97..108, y 189..202.
    let bend: f32 = (97..=108)
        .flat_map(|x| (189..=202).map(move |y| alpha(x, y)))
        .sum();

    let total: f32 = bytes.chunks(4).map(|p| p[3] as f32 / 255.0).sum();
    eprintln!("{label}: total_alpha = {total:.3} px^2");
    eprintln!(
        "  vert run  x=97..103 : [{}]  effective width {vert_width:.4} px",
        join(&vert_cols)
    );
    eprintln!(
        "  horiz run y=198..202: [{}]  effective width {horiz_width:.4} px",
        join(&horiz_rows)
    );
    eprintln!("  bend area (x97..108, y189..202): {bend:.3} px^2");

    // AA profile at one mid-segment position: per-column alpha across the
    // vertical stroke's edges (y=100) and per-row alpha across the
    // horizontal stroke's edges (x=130).
    let vert_aa: Vec<f32> = (96..=104).map(|x| alpha(x, 100)).collect();
    let horiz_aa: Vec<f32> = (196..=204).map(|y| alpha(130, y)).collect();
    eprintln!("profile sc={sample_count} vert: [{}]", join(&vert_aa));
    eprintln!("profile sc={sample_count} horiz: [{}]", join(&horiz_aa));
}

#[test]
fn path_rasterization_lane_coverage() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("no wgpu adapter available");
    let adapter_info = adapter.get_info();
    eprintln!(
        "adapter: {} ({:?})",
        adapter_info.name, adapter_info.backend
    );

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("repro_device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
    }))
    .expect("request_device failed");
    device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
        eprintln!("device error: {error}");
    }));

    let format = wgpu::TextureFormat::Bgra8Unorm;
    // 256 keeps `bytes_per_row = width * 4` a multiple of 256, as required
    // by `copy_texture_to_buffer`.
    let (width, height) = (256u32, 240u32);
    let path = lane_path(width, height);
    eprintln!("{} path vertices", path.vertices.len());

    // A color attachment accepts 1 and 4 samples without any device feature;
    // 2 and 8 additionally require TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
    // which this device does not enable — skip such counts instead of dying
    // in device validation.
    let msaa_ok = |n: u32| {
        n == 1
            || n == 4
            || (device
                .features()
                .contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES)
                && adapter
                    .get_texture_format_features(format)
                    .flags
                    .sample_count_supported(n))
    };
    for samples in [4u32, 2, 1] {
        if !msaa_ok(samples) {
            eprintln!("skipping sample_count {samples}: unsupported by this device");
            continue;
        }
        let (intermediate, final_) = rasterize(
            &instance, &device, &queue, format, width, height, samples, &path,
        );
        eprintln!("== sample_count {samples} ==");
        report(
            samples,
            "intermediate (post-resolve)",
            &intermediate,
            width,
            height,
        );
        report(samples, "final (post-blit)", &final_, width, height);
    }
}

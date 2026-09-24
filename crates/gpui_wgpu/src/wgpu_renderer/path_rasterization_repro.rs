//! Headless repro of the path rasterization pipeline
//! (`draw_paths_to_intermediate` + `draw_paths_from_intermediate`) for
//! diagnosing the GitGraph lane aliasing on near-horizontal segments.
//!
//! It rasterizes the exact lane geometry used by
//! `lane_tessellation_width` in `gpui::path_builder` (vertical run ->
//! quadratic bend -> horizontal run, 1.5px stroke), scaled like
//! `Window::paint_path` at scale factor 2 (3 device px), through the same
//! WGSL entry points, intermediate texture, MSAA, and blit pass that the
//! production renderer uses. It sweeps the (sample_count,
//! STROKE_ANALYTIC_AA) matrix — (4,0) = today's production, (4,1) and
//! (1,1) = the candidates — reports per-column / per-row coverage profiles
//! for the vertical run, the horizontal run, and the bend, and dumps
//! 4x-magnified BMPs of the bend to `/home/emil/mono/.tmp/`.

use super::*;
use gpui::{ContentMask, Hsla, PathBuilder, ScaledPixels, point, px, solid_background};
use std::num::NonZeroU64;

/// The exact GitGraph "checkout curve" geometry from
/// `lane_tessellation_width`: vertical run -> quadratic bend -> horizontal
/// run, stroked at 1.5px.
fn lane_path(width: u32, height: u32, scale: f32) -> Path<ScaledPixels> {
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
    // The exact `Window::paint_path` sequence: logical geometry, then
    // `path.scale(scale_factor)`, which scales vertices and the content
    // mask but not the normalized `st_position` — at scale 2 the 1.5px
    // stroke is 3 device px with the same ±1 normalization.
    path.scale(scale)
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
    stroke_analytic: bool,
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
    // Decoupled from `sample_count` on purpose: the matrix under test is
    // (sample_count, STROKE_ANALYTIC_AA), including combinations the
    // production gate in `WgpuRenderer::create_pipelines` never creates.
    let stroke_aa = [(
        "STROKE_ANALYTIC_AA",
        if stroke_analytic { 1.0 } else { 0.0 },
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
        // NOTE: conservative rasterization must stay OFF here to mirror
        // production: with it on, a quad's diagonal clipping a pixel makes
        // BOTH triangles generate fragments and their edge alphas double-
        // blend (0.5 + 0.5 -> 0.75), masking the real profiles.
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

/// Per-region coverage profiles for the lane geometry, all regions in
/// device pixels = logical coordinate x `s` (the lane's logical layout is
/// x0=100, y_top=50, row y=200).
fn report(
    sample_count: u32,
    analytic: bool,
    label: &str,
    bytes: &[u8],
    width: u32,
    height: u32,
    s: f32,
) {
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
    let range = |a: f32, b: f32| ((a * s) as u32)..=((b * s) as u32 + 1);

    // Vertical run: stroke centered at x=100s; rows 55s..187s skip the ends.
    let vert_cols: Vec<f32> = range(97., 103.)
        .map(|x| range(55., 187.).map(|y| alpha(x, y)).sum())
        .collect();
    let vert_width: f32 = vert_cols.iter().sum::<f32>() / (133.0 * s);

    // Horizontal run: stroke centered at y=200s; columns 115s..145s.
    let horiz_rows: Vec<f32> = range(198., 202.)
        .map(|y| range(115., 145.).map(|x| alpha(x, y)).sum())
        .collect();
    let horiz_width: f32 = horiz_rows.iter().sum::<f32>() / (31.0 * s);

    // Bend: the quadratic and both joins.
    let bend: f32 = range(97., 108.)
        .flat_map(|x| range(189., 202.).map(move |y| alpha(x, y)))
        .sum();

    let total: f32 = bytes.chunks(4).map(|p| p[3] as f32 / 255.0).sum();
    eprintln!("{label}: total_alpha = {total:.3} px^2");
    eprintln!(
        "  vert run  x{}..{} : [{}]  effective width {vert_width:.4} px",
        (97.0 * s) as u32,
        (103.0 * s) as u32,
        join(&vert_cols)
    );
    eprintln!(
        "  horiz run y{}..{}: [{}]  effective width {horiz_width:.4} px",
        (198.0 * s) as u32,
        (202.0 * s) as u32,
        join(&horiz_rows)
    );
    eprintln!("  bend area: {bend:.3} px^2");

    // AA profile at one mid-segment position: per-column alpha across the
    // vertical stroke's edges (y=100s) and per-row alpha across the
    // horizontal stroke's edges (x=130s).
    let vert_aa: Vec<f32> = range(96., 104.)
        .map(|x| alpha(x, (100.0 * s) as u32))
        .collect();
    let horiz_aa: Vec<f32> = range(196., 204.)
        .map(|y| alpha((130.0 * s) as u32, y))
        .collect();
    let aa_flag = if analytic { "analytic" } else { "geometric" };
    eprintln!(
        "profile sc={sample_count} {aa_flag} vert: [{}]",
        join(&vert_aa)
    );
    eprintln!(
        "profile sc={sample_count} {aa_flag} horiz: [{}]",
        join(&horiz_aa)
    );
}

/// 24-bit BMP of a magnified ROI of `bytes` (RGBA, premultiplied),
/// composited over a dark panel background so it matches what Zed shows.
fn write_bmp(out: &str, bytes: &[u8], width: u32, x0: u32, y0: u32, rw: u32, rh: u32, mag: u32) {
    const BG: [f32; 3] = [0.118, 0.118, 0.180]; // ~#1e1e2e
    let (ow, oh) = (
        (rw as usize) * (mag as usize),
        (rh as usize) * (mag as usize),
    );
    let mut img = vec![0u8; ow * oh * 3];
    for py in 0..rh as usize {
        for px in 0..rw as usize {
            let i = ((y0 + py as u32) as usize * width as usize + (x0 + px as u32) as usize) * 4;
            let a = bytes[i + 3] as f32 / 255.0;
            let fg = [0, 1, 2].map(|c| bytes[i + c] as f32 / 255.0); // premultiplied
            let rgb: [u8; 3] = [0, 1, 2]
                .map(|c| (fg[c] + BG[c] * (1.0 - a)) * 255.0)
                .map(|v| v.clamp(0.0, 255.0).round() as u8);
            for dy in 0..mag as usize {
                for dx in 0..mag as usize {
                    let o = ((py * mag as usize + dy) * ow + (px * mag as usize + dx)) * 3;
                    img[o..o + 3].copy_from_slice(&rgb);
                }
            }
        }
    }
    let row_bytes = (ow * 3 + 3) & !3;
    let mut bmp = Vec::with_capacity(54 + row_bytes * oh);
    let put = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    let put16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    bmp.extend_from_slice(b"BM");
    put(&mut bmp, 54 + (row_bytes * oh) as u32); // file size
    put16(&mut bmp, 0); // reserved
    put16(&mut bmp, 0); // reserved
    put(&mut bmp, 54); // pixel data offset
    put(&mut bmp, 40); // BITMAPINFOHEADER size
    put(&mut bmp, ow as u32);
    put(&mut bmp, oh as u32);
    put16(&mut bmp, 1); // planes
    put16(&mut bmp, 24); // bits per pixel
    put(&mut bmp, 0); // compression (BI_RGB)
    put(&mut bmp, (row_bytes * oh) as u32); // image size
    put(&mut bmp, 2835); // x pixels per meter
    put(&mut bmp, 2835); // y pixels per meter
    put(&mut bmp, 0); // colors used
    put(&mut bmp, 0); // important colors
    for row in (0..oh).rev() {
        bmp.extend_from_slice(&img[row * ow * 3..(row + 1) * ow * 3]);
        bmp.extend_from_slice(&vec![0u8; (row_bytes - ow * 3) as usize]);
    }
    std::fs::write(out, &bmp).expect("write bmp");
    eprintln!("wrote {out} ({ow}x{oh})");
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
    // 512 keeps `bytes_per_row = width * 4` a multiple of 256, as required
    // by `copy_texture_to_buffer`.
    let (width, height) = (512u32, 480u32);
    // The production path at Emil's scale factor 2.
    let path = lane_path(256, 240, 2.0);
    eprintln!("{} path vertices", path.vertices.len());
    // Vertex dump: ground truth for the tessellation. The horizontal run is
    // the contour from (105.33, 200) to (155.33, 200) logical — device x
    // 210..311, y 398..402; the bend/curve is x 196..213.
    for v in &path.vertices {
        let x = v.xy_position.x.0;
        let y = v.xy_position.y.0;
        if (196.0..312.0).contains(&x) && (380.0..420.0).contains(&y)
            || (195.0..205.0).contains(&x) && (90.0..390.0).contains(&y)
        {
            eprintln!(
                "  v xy=({:.3},{:.3}) st=({:.4},{:.4})",
                x, y, v.st_position.x, v.st_position.y
            );
        }
    }

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
    // The (sample_count, STROKE_ANALYTIC_AA) matrix. Production runs
    // (4, true): MSAA composed with the analytic stroke coverage — exact
    // because the dilated geometry keeps MSAA coverage at 1.0 everywhere
    // the analytic ramp is nonzero. (1, true) proves sample-count
    // independence; (4, false) is a diagnostic only (dilated geometry
    // without analytic: the dilated band paints solid, no production
    // config looks like this).
    let cases: [(u32, bool, &str); 3] = [
        (4, true, "sc4_production"),
        (1, true, "sc1_analytic"),
        (4, false, "sc4_dilated_geometric_diagnostic"),
    ];
    for (samples, analytic, name) in cases {
        if !msaa_ok(samples) {
            eprintln!("skipping {name}: sample_count {samples} unsupported");
            continue;
        }
        let (intermediate, final_) = rasterize(
            &instance, &device, &queue, format, width, height, samples, analytic, &path,
        );
        eprintln!("== {name} (sample_count {samples}) ==");
        report(
            samples,
            analytic,
            "intermediate (post-resolve)",
            &intermediate,
            width,
            height,
            2.0,
        );
        report(
            samples,
            analytic,
            "final (post-blit)",
            &final_,
            width,
            height,
            2.0,
        );
        // 4x-magnified ROI around the bend (device x180..360, y80..420).
        write_bmp(
            &format!("/home/emil/mono/.tmp/repro_{name}.bmp"),
            &final_,
            width,
            180,
            80,
            180,
            340,
            4,
        );
    }
}

/// Regression probe for the stroke edge-row fix: stroke geometry is dilated
/// ~1 logical px at tessellation (see `STROKE_AA_DILATION` in gpui's
/// `path_builder`), so a fragment exists for every pixel whose center is
/// within the 0.5-device-px analytic ramp, and the edge rows read their
/// exact box-filter coverage. Rasterizes a bare two-vertex horizontal line
/// (no bend, no joins) through the same pipeline helpers as
/// `path_rasterization_lane_coverage` and prints the per-row alpha profile
/// (rows 396..403, y down, row r covers device y [r, r+1)) at one column,
/// for two configs x two cases:
/// - on-center:  band y [398.5, 401.5]  -> expect [0.5, 1, 1, 0.5]
/// - off-center: band y [398.75, 401.75] -> expect [0.25, 1, 1, 0.75]
///
/// Pre-fix (undilated geometry), the on-center bottom row read 0.0: the GPU
/// generates no fragment when the pixel center sits exactly on the quad's
/// bottom edge (edge-function tie-break), and the off-center top row
/// (center outside the quad) read 0.0 the same way.
#[test]
fn path_rasterization_hline_probe() {
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
    eprintln!("adapter: {}", adapter.get_info().name);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("hline_probe_device"),
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
    // Same canvas as the lane test (512 keeps bytes_per_row a multiple of
    // 256 for `copy_texture_to_buffer`).
    let (width, height) = (512u32, 480u32);
    let scale = 2.0;
    let x_sample = 250u32; // inside the line's device x range 200..300

    // Bare horizontal line: move_to + line_to, stroked at 1.5 logical px
    // (3 device px at scale 2), built and scaled exactly like `lane_path`
    // (logical geometry, then `path.scale(scale)`).
    let hline = |yc_logical: f32| -> Path<ScaledPixels> {
        let x0 = 100.0;
        let x1 = 150.0;
        let mut builder = PathBuilder::stroke(px(1.5));
        builder.move_to(point(px(x0), px(yc_logical)));
        builder.line_to(point(px(x1), px(yc_logical)));
        let mut path = builder.build().unwrap();
        // Full-viewport content mask, as in `lane_path` (without it
        // `clipped_bounds` is empty and nothing is rasterized).
        path.content_mask = ContentMask {
            bounds: Bounds {
                origin: Point::default(),
                size: Size {
                    width: px(width as f32 / scale),
                    height: px(height as f32 / scale),
                },
            },
        };
        path.color = solid_background(Hsla {
            h: 0.0,
            s: 0.0,
            l: 1.0,
            a: 1.0,
        });
        path.scale(scale)
    };

    let alpha =
        |bytes: &[u8], x: u32, y: u32| bytes[((y * width + x) as usize) * 4 + 3] as f32 / 255.0;
    let profile = |bytes: &[u8]| -> String {
        (396..=403)
            .map(|y| format!("{:.3}", alpha(bytes, x_sample, y)))
            .collect::<Vec<_>>()
            .join(" ")
    };

    // (label, sample_count, STROKE_ANALYTIC_AA, yc in logical px).
    let cases: [(&str, u32, bool, f32); 4] = [
        ("on-center  sc4-geometric", 4, false, 200.0),
        ("on-center  sc1-analytic", 1, true, 200.0),
        ("off-center sc4-geometric", 4, false, 200.125),
        ("off-center sc1-analytic", 1, true, 200.125),
    ];
    for (name, samples, analytic, yc_logical) in cases {
        let path = hline(yc_logical);
        eprintln!(
            "{name}: {} vertices: {}",
            path.vertices.len(),
            path.vertices
                .iter()
                .map(|v| format!("({:.2},{:.2})", v.xy_position.x.0, v.xy_position.y.0))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let (intermediate, final_) = rasterize(
            &instance, &device, &queue, format, width, height, samples, analytic, &path,
        );
        eprintln!(
            "{name}: band y={:.2}..{:.2} | rows 396..403 @x={x_sample}\n  intermediate: [{}]\n  final:        [{}]",
            yc_logical * scale - 1.5,
            yc_logical * scale + 1.5,
            profile(&intermediate),
            profile(&final_)
        );
    }
}

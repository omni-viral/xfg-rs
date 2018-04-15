#![deny(unused_must_use)]
// #![allow(dead_code)]

extern crate genmesh;
extern crate smallvec;
extern crate xfg_examples;
use xfg_examples::*;

use std::borrow::Borrow;
use std::iter::once;
use std::ops::{Add, BitOr, Sub};
use std::sync::Arc;

use cgmath::{EuclideanSpace, Matrix4, Point3, Transform};

use gfx_hal::{Backend, Device, IndexType};
use gfx_hal::buffer::{IndexBufferView, Usage};
use gfx_hal::command::{ClearColor, ClearDepthStencil, CommandBuffer, Primary,
                       RenderPassInlineEncoder};
use gfx_hal::device::ShaderError;
use gfx_hal::format::{Aspects, Format, Swizzle};
use gfx_hal::image::{Access, Layout, SubresourceRange, ViewKind};
use gfx_hal::memory::{cast_slice, Barrier, Dependencies, Pod};
use gfx_hal::pso::{BlendState, ColorBlendDesc, ColorMask, Descriptor, DescriptorSetLayoutBinding,
                   DescriptorSetWrite, DescriptorType, ElemStride, Element, EntryPoint,
                   GraphicsShaderSet, PipelineStage, ShaderStageFlags, VertexBufferSet};
use gfx_hal::queue::Transfer;
use mem::{Block, Factory, SmartAllocator};
use smallvec::SmallVec;
use xfg::{ColorAttachment, DepthStencilAttachment, DescriptorPool, GraphBuilder, Pass, PassDesc,
          PassShaders};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct ObjectData {
    albedo: [f32; 3],
    metallic: f32,
    emission: [f32; 3],
    roughness: f32,
    ambient_occlusion: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PosNormal {
    position: [f32; 3],
    normal: [f32; 3],
}

unsafe impl Pod for PosNormal {}

#[derive(Debug)]
struct DrawPbmPrepare;
impl PassDesc for DrawPbmPrepare {
    fn name(&self) -> &str {
        "DrawPbmPrepare"
    }

    fn sampled(&self) -> usize {
        0
    }

    fn storage(&self) -> usize {
        0
    }

    fn inputs(&self) -> usize {
        0
    }

    fn colors(&self) -> usize {
        4
    }

    fn depth(&self) -> bool {
        true
    }

    fn stencil(&self) -> bool {
        false
    }

    fn vertices(&self) -> &[(&[Element<Format>], ElemStride)] {
        &[
            (
                &[
                    Element {
                        format: Format::Rgb32Float,
                        offset: 0,
                    },
                    Element {
                        format: Format::Rgb32Float,
                        offset: 12,
                    },
                ],
                24,
            ),
        ]
    }

    fn bindings(&self) -> &[DescriptorSetLayoutBinding] {
        &[
            DescriptorSetLayoutBinding {
                binding: 0,
                ty: DescriptorType::UniformBuffer,
                count: 1,
                stage_flags: ShaderStageFlags::VERTEX,
            },
            DescriptorSetLayoutBinding {
                binding: 1,
                ty: DescriptorType::UniformBuffer,
                count: 1,
                stage_flags: ShaderStageFlags::FRAGMENT,
            },
        ]
    }
}

impl<B> PassShaders<B> for DrawPbmPrepare
where
    B: Backend,
{
    fn shaders<'a>(
        &self,
        shaders: &'a mut SmallVec<[B::ShaderModule; 5]>,
        device: &B::Device,
    ) -> Result<GraphicsShaderSet<'a, B>, ShaderError> {
        shaders.clear();
        shaders.push(device.create_shader_module(include_bytes!("first.vert.spv"))?);
        shaders.push(device.create_shader_module(include_bytes!("first.frag.spv"))?);

        Ok(GraphicsShaderSet {
            vertex: EntryPoint {
                entry: "main",
                module: &shaders[0],
                specialization: &[],
            },
            hull: None,
            domain: None,
            geometry: None,
            fragment: Some(EntryPoint {
                entry: "main",
                module: &shaders[1],
                specialization: &[],
            }),
        })
    }
}

impl<B> Pass<B, Scene<B, ObjectData>> for DrawPbmPrepare
where
    B: Backend,
{
    fn prepare<'a>(
        &mut self,
        pool: &mut DescriptorPool<B>,
        cbuf: &mut CommandBuffer<B, Transfer>,
        device: &B::Device,
        _inputs: &[&B::Image],
        frame: usize,
        scene: &mut Scene<B, ObjectData>,
    ) {
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        struct VertexArgs {
            proj: Matrix4<f32>,
            view: Matrix4<f32>,
            model: Matrix4<f32>,
        }

        unsafe impl Pod for VertexArgs {}

        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        struct FragmentArgs {
            albedo: [f32; 3],
            roughness: f32,
            emission: [f32; 3],
            metallic: f32,
            ambient_occlusion: f32,
        }

        unsafe impl Pod for FragmentArgs {}

        let ref mut allocator = scene.allocator;
        let view = scene.camera.transform.inverse_transform().unwrap();
        // Update uniform cache
        for obj in &mut scene.objects {
            let vertex_args = VertexArgs {
                model: obj.transform,
                proj: scene.camera.projection,
                view,
            };

            let fragment_args = FragmentArgs {
                albedo: obj.data.albedo,
                metallic: obj.data.metallic,
                emission: obj.data.emission,
                roughness: obj.data.roughness,
                ambient_occlusion: obj.data.ambient_occlusion,
            };

            let vertex_args_range = Some(0)..Some(::std::mem::size_of::<VertexArgs>() as u64);
            let fragment_args_offset = shift_for_alignment(256, vertex_args_range.end.unwrap());
            let fragment_args_range = Some(fragment_args_offset)
                ..Some(fragment_args_offset + ::std::mem::size_of::<FragmentArgs>() as u64);

            let grow = (obj.cache.len()..frame + 1).map(|_| None);
            obj.cache.extend(grow);
            let cache = obj.cache[frame].get_or_insert_with(|| {
                let buffer = allocator
                    .create_buffer(
                        device,
                        REQUEST_DEVICE_LOCAL,
                        fragment_args_range.end.unwrap(),
                        Usage::UNIFORM | Usage::TRANSFER_DST,
                    )
                    .unwrap();
                let set = pool.allocate(device);
                device.write_descriptor_sets(
                    once(DescriptorSetWrite {
                        set: &set,
                        binding: 0,
                        array_offset: 0,
                        descriptors: Some(Descriptor::Buffer(
                            buffer.borrow(),
                            vertex_args_range.clone(),
                        )),
                    }).chain(once(DescriptorSetWrite {
                        set: &set,
                        binding: 1,
                        array_offset: 0,
                        descriptors: Some(Descriptor::Buffer(
                            buffer.borrow(),
                            fragment_args_range.clone(),
                        )),
                    })),
                );
                Cache {
                    uniforms: vec![buffer],
                    views: vec![],
                    set,
                }
            });
            cbuf.update_buffer(
                cache.uniforms[0].borrow(),
                vertex_args_range.start.unwrap(),
                cast_slice(&[vertex_args]),
            );
            cbuf.update_buffer(
                cache.uniforms[0].borrow(),
                fragment_args_range.start.unwrap(),
                cast_slice(&[fragment_args]),
            );
        }
    }

    fn draw_inline<'a>(
        &mut self,
        layout: &B::PipelineLayout,
        mut encoder: RenderPassInlineEncoder<B, Primary>,
        _device: &B::Device,
        _inputs: &[&B::Image],
        frame: usize,
        scene: &Scene<B, ObjectData>,
    ) {
        for object in &scene.objects {
            encoder.bind_graphics_descriptor_sets(
                layout,
                0,
                Some(&object.cache[frame].as_ref().unwrap().set),
            );
            encoder.bind_index_buffer(IndexBufferView {
                buffer: object.mesh.indices.borrow(),
                offset: 0,
                index_type: IndexType::U16,
            });
            encoder.bind_vertex_buffers(VertexBufferSet(vec![(object.mesh.vertices.borrow(), 0)]));
            encoder.draw_indexed(0..object.mesh.index_count, 0, 0..1);
        }
    }

    fn cleanup(
        &mut self,
        pool: &mut DescriptorPool<B>,
        device: &B::Device,
        scene: &mut Scene<B, ObjectData>,
    ) {
        for object in &mut scene.objects {
            for cache in object.cache.drain(..) {
                if let Some(cache) = cache {
                    pool.free(cache.set);
                    for uniform in cache.uniforms {
                        scene.allocator.destroy_buffer(device, uniform);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct DrawPbmShade;
impl PassDesc for DrawPbmShade {
    fn name(&self) -> &str {
        "DrawPbmShade"
    }

    fn sampled(&self) -> usize {
        0
    }

    fn storage(&self) -> usize {
        4
    }

    fn inputs(&self) -> usize {
        0
    }

    fn colors(&self) -> usize {
        1
    }

    fn depth(&self) -> bool {
        false
    }

    fn stencil(&self) -> bool {
        false
    }

    fn vertices(&self) -> &[(&[Element<Format>], ElemStride)] {
        &[]
    }

    fn bindings(&self) -> &[DescriptorSetLayoutBinding] {
        &[
            DescriptorSetLayoutBinding {
                binding: 0,
                ty: DescriptorType::StorageImage,
                count: 1,
                stage_flags: ShaderStageFlags::FRAGMENT,
            },
            DescriptorSetLayoutBinding {
                binding: 1,
                ty: DescriptorType::StorageImage,
                count: 1,
                stage_flags: ShaderStageFlags::FRAGMENT,
            },
            DescriptorSetLayoutBinding {
                binding: 2,
                ty: DescriptorType::StorageImage,
                count: 1,
                stage_flags: ShaderStageFlags::FRAGMENT,
            },
            DescriptorSetLayoutBinding {
                binding: 3,
                ty: DescriptorType::StorageImage,
                count: 1,
                stage_flags: ShaderStageFlags::FRAGMENT,
            },
            DescriptorSetLayoutBinding {
                binding: 4,
                ty: DescriptorType::UniformBuffer,
                count: 1,
                stage_flags: ShaderStageFlags::FRAGMENT,
            },
        ]
    }
}

impl<B> PassShaders<B> for DrawPbmShade
where
    B: Backend,
{
    fn shaders<'a>(
        &self,
        shaders: &'a mut SmallVec<[B::ShaderModule; 5]>,
        device: &B::Device,
    ) -> Result<GraphicsShaderSet<'a, B>, ShaderError> {
        shaders.clear();
        shaders.push(device.create_shader_module(include_bytes!("second.vert.spv"))?);
        shaders.push(device.create_shader_module(include_bytes!("second.frag.spv"))?);

        Ok(GraphicsShaderSet {
            vertex: EntryPoint {
                entry: "main",
                module: &shaders[0],
                specialization: &[],
            },
            hull: None,
            domain: None,
            geometry: None,
            fragment: Some(EntryPoint {
                entry: "main",
                module: &shaders[1],
                specialization: &[],
            }),
        })
    }
}

impl<B> Pass<B, Scene<B, ObjectData>> for DrawPbmShade
where
    B: Backend,
{
    fn prepare<'a>(
        &mut self,
        pool: &mut DescriptorPool<B>,
        cbuf: &mut CommandBuffer<B, Transfer>,
        device: &B::Device,
        inputs: &[&B::Image],
        frame: usize,
        scene: &mut Scene<B, ObjectData>,
    ) {
        assert_eq!(4, inputs.len());

        #[derive(Clone, Copy, Debug, PartialEq)]
        struct FragmentArgs {
            light_position: [f32; 3],
            _pad0: f32,
            color: [f32; 3],
            _pad1: f32,
            camera_position: [f32; 3],
            _pad2: f32,
            ambient_light: [f32; 3],
            _pad3: f32,
        }

        unsafe impl Pod for FragmentArgs {}

        let ref mut allocator = scene.allocator;
        let camera_position = scene
            .camera
            .transform
            .transform_point(Point3::origin())
            .into();

        // Update uniform cache
        for light in &mut scene.lights {
            let fragment_args = FragmentArgs {
                light_position: light.transform.transform_point(Point3::origin()).into(),
                color: light.color,
                camera_position,
                ambient_light: scene.ambient.0,
                _pad0: 0.0,
                _pad1: 0.0,
                _pad2: 0.0,
                _pad3: 0.0,
            };

            let color_range = SubresourceRange {
                aspects: Aspects::COLOR,
                levels: 0..1,
                layers: 0..1,
            };

            let size = ::std::mem::size_of::<FragmentArgs>() as u64;

            let grow = (light.cache.len()..frame + 1).map(|_| None);
            light.cache.extend(grow);
            let cache = light.cache[frame].get_or_insert_with(|| {
                let views = vec![
                    device
                        .create_image_view(
                            inputs[0],
                            ViewKind::D2,
                            Format::Rgba32Float,
                            Swizzle::NO,
                            color_range.clone(),
                        )
                        .unwrap(),
                    device
                        .create_image_view(
                            inputs[1],
                            ViewKind::D2,
                            Format::Rgba32Float,
                            Swizzle::NO,
                            color_range.clone(),
                        )
                        .unwrap(),
                    device
                        .create_image_view(
                            inputs[2],
                            ViewKind::D2,
                            Format::Rgba32Float,
                            Swizzle::NO,
                            color_range.clone(),
                        )
                        .unwrap(),
                    device
                        .create_image_view(
                            inputs[3],
                            ViewKind::D2,
                            Format::Rgba32Float,
                            Swizzle::NO,
                            color_range.clone(),
                        )
                        .unwrap(),
                ];
                let buffer = allocator
                    .create_buffer(
                        device,
                        REQUEST_DEVICE_LOCAL,
                        size,
                        Usage::UNIFORM | Usage::TRANSFER_DST,
                    )
                    .unwrap();
                let set = pool.allocate(device);
                device.write_descriptor_sets(
                    once(DescriptorSetWrite {
                        set: &set,
                        binding: 0,
                        array_offset: 0,
                        descriptors: Some(Descriptor::Image(&views[0], Layout::General)),
                    }).chain(once(DescriptorSetWrite {
                        set: &set,
                        binding: 1,
                        array_offset: 0,
                        descriptors: Some(Descriptor::Image(&views[1], Layout::General)),
                    }))
                        .chain(once(DescriptorSetWrite {
                            set: &set,
                            binding: 2,
                            array_offset: 0,
                            descriptors: Some(Descriptor::Image(&views[2], Layout::General)),
                        }))
                        .chain(once(DescriptorSetWrite {
                            set: &set,
                            binding: 3,
                            array_offset: 0,
                            descriptors: Some(Descriptor::Image(&views[3], Layout::General)),
                        }))
                        .chain(once(DescriptorSetWrite {
                            set: &set,
                            binding: 4,
                            array_offset: 0,
                            descriptors: Some(Descriptor::Buffer(
                                buffer.borrow(),
                                Some(0)..Some(size),
                            )),
                        })),
                );
                Cache {
                    uniforms: vec![buffer],
                    views,
                    set,
                }
            });

            let states = (Access::COLOR_ATTACHMENT_WRITE, Layout::General)
                ..(Access::SHADER_READ, Layout::General);
            cbuf.pipeline_barrier(
                PipelineStage::COLOR_ATTACHMENT_OUTPUT..PipelineStage::FRAGMENT_SHADER,
                Dependencies::empty(),
                &[
                    Barrier::Image {
                        states: states.clone(),
                        target: inputs[0],
                        range: color_range.clone(),
                    },
                    Barrier::Image {
                        states: states.clone(),
                        target: inputs[1],
                        range: color_range.clone(),
                    },
                    Barrier::Image {
                        states: states.clone(),
                        target: inputs[2],
                        range: color_range.clone(),
                    },
                    Barrier::Image {
                        states: states.clone(),
                        target: inputs[3],
                        range: color_range.clone(),
                    },
                ],
            );

            cbuf.update_buffer(cache.uniforms[0].borrow(), 0, cast_slice(&[fragment_args]));
        }
    }

    fn draw_inline<'a>(
        &mut self,
        layout: &B::PipelineLayout,
        mut encoder: RenderPassInlineEncoder<B, Primary>,
        _device: &B::Device,
        _inputs: &[&B::Image],
        frame: usize,
        scene: &Scene<B, ObjectData>,
    ) {
        for light in &scene.lights {
            encoder.bind_graphics_descriptor_sets(
                layout,
                0,
                Some(&light.cache[frame].as_ref().unwrap().set),
            );
            encoder.draw(0..6, 0..1);
        }
    }

    fn cleanup(
        &mut self,
        pool: &mut DescriptorPool<B>,
        device: &B::Device,
        scene: &mut Scene<B, ObjectData>,
    ) {
        for object in &mut scene.objects {
            for cache in object.cache.drain(..) {
                if let Some(cache) = cache {
                    pool.free(cache.set);
                    for uniform in cache.uniforms {
                        scene.allocator.destroy_buffer(device, uniform);
                    }
                }
            }
        }

        for light in &mut scene.lights {
            for cache in light.cache.drain(..) {
                if let Some(cache) = cache {
                    pool.free(cache.set);
                    for uniform in cache.uniforms {
                        scene.allocator.destroy_buffer(device, uniform);
                    }
                }
            }
        }
    }
}

type AnyPass = Box<Pass<back::Backend, Scene<back::Backend, ObjectData>>>;

fn graph<'a>(surface_format: Format, graph: &mut GraphBuilder<AnyPass>) {
    let ambient_roughness = graph.add_attachment(
        ColorAttachment::new(Format::Rgba32Float)
            .with_clear(ClearColor::Float([0.0, 0.0, 0.0, 0.0])),
    );
    let emission_metallic = graph.add_attachment(
        ColorAttachment::new(Format::Rgba32Float)
            .with_clear(ClearColor::Float([0.0, 0.0, 0.0, 0.0])),
    );
    let normal_normal_ambient_occlusion = graph.add_attachment(
        ColorAttachment::new(Format::Rgba32Float)
            .with_clear(ClearColor::Float([0.0, 0.0, 0.0, 0.0])),
    );
    let position_depth = graph.add_attachment(
        ColorAttachment::new(Format::Rgba32Float)
            .with_clear(ClearColor::Float([0.0, 0.0, 0.0, 0.0])),
    );
    let present = graph.add_attachment(
        ColorAttachment::new(surface_format).with_clear(ClearColor::Float([0.0, 0.0, 0.0, 1.0])),
    );
    let depth = graph.add_attachment(
        DepthStencilAttachment::new(Format::D32Float).with_clear(ClearDepthStencil(1.0, 0)),
    );

    let prepare = AnyPass::from(Box::new(DrawPbmPrepare))
        .build()
        .with_color(ambient_roughness)
        .with_color(emission_metallic)
        .with_color(normal_normal_ambient_occlusion)
        .with_color(position_depth)
        .with_depth_stencil(depth);

    let shade = AnyPass::from(Box::new(DrawPbmShade))
        .build()
        .with_storage(ambient_roughness)
        .with_storage(emission_metallic)
        .with_storage(normal_normal_ambient_occlusion)
        .with_storage(position_depth)
        .with_color_blend(present, ColorBlendDesc(ColorMask::ALL, BlendState::ADD));

    graph.add_pass(prepare).add_pass(shade).set_present(present);
}

fn fill<B>(scene: &mut Scene<B, ObjectData>, device: &B::Device)
where
    B: Backend,
{
    scene.camera.transform = Matrix4::from_translation([0.0, 0.0, 15.0].into());

    let mut data = ObjectData {
        albedo: [1.0; 3],
        metallic: 0.0,
        emission: [0.0, 0.0, 0.0],
        roughness: 0.0,
        ambient_occlusion: 1.0,
    };

    let sphere = Arc::new(create_sphere(device, &mut scene.allocator));

    for i in 0..6 {
        for j in 0..6 {
            let transform = Matrix4::from_translation(
                [2.5 * (i as f32) - 6.25, 2.5 * (j as f32) - 6.25, 0.0].into(),
            );
            data.metallic = j as f32 * 0.2;
            data.roughness = i as f32 * 0.2;
            scene.objects.push(Object {
                mesh: sphere.clone(),
                data,
                transform,
                cache: Vec::new(),
            });
        }
    }

    scene.lights.push(Light {
        color: [0.0, 0.623529411764706, 0.419607843137255],
        transform: Matrix4::from_translation([-6.25, -6.25, 10.0].into()),
        cache: Vec::new(),
    });

    scene.lights.push(Light {
        color: [0.768627450980392, 0.007843137254902, 0.2],
        transform: Matrix4::from_translation([6.25, -6.25, 10.0].into()),
        cache: Vec::new(),
    });

    scene.lights.push(Light {
        color: [1.0, 0.827450980392157, 0.0],
        transform: Matrix4::from_translation([-6.25, 6.25, 10.0].into()),
        cache: Vec::new(),
    });

    scene.lights.push(Light {
        color: [0.0, 0.529411764705882, 0.741176470588235],
        transform: Matrix4::from_translation([6.25, 6.25, 10.0].into()),
        cache: Vec::new(),
    });
}

fn main() {
    run(graph, fill);
}

fn create_sphere<B>(device: &B::Device, factory: &mut SmartAllocator<B>) -> Mesh<B>
where
    B: Backend,
{
    use genmesh::{EmitTriangles, Triangle};
    use genmesh::generators::{IndexedPolygon, SharedVertex, SphereUV};

    let sphere = SphereUV::new(40, 20);

    let vertices = sphere
        .shared_vertex_iter()
        .map(|v| PosNormal {
            position: v.pos,
            normal: v.normal,
        })
        .collect::<Vec<_>>();

    let vertices: &[u8] = cast_slice(&vertices);

    let buffer = factory
        .create_buffer(
            device,
            REQUEST_CPU_VISIBLE,
            vertices.len() as u64,
            Usage::VERTEX,
        )
        .unwrap();
    {
        let start = buffer.range().start;
        let end = start + vertices.len() as u64;
        let mut writer = device
            .acquire_mapping_writer(buffer.memory(), start..end)
            .unwrap();
        writer.copy_from_slice(vertices);
        device.release_mapping_writer(writer);
    }

    let vertices = buffer;

    let indices = sphere
        .indexed_polygon_iter()
        .flat_map(|polygon| {
            let mut indices = SmallVec::<[u16; 6]>::new();
            polygon.emit_triangles(|Triangle { x, y, z }| {
                indices.push(x as u16);
                indices.push(y as u16);
                indices.push(z as u16);
            });
            indices
        })
        .collect::<Vec<_>>();

    let index_count = indices.len() as u32;

    let indices: &[u8] = cast_slice(&indices);

    let buffer = factory
        .create_buffer(
            device,
            REQUEST_CPU_VISIBLE,
            indices.len() as u64,
            Usage::INDEX,
        )
        .unwrap();
    {
        let start = buffer.range().start;
        let end = start + indices.len() as u64;
        let mut writer = device
            .acquire_mapping_writer(buffer.memory(), start..end)
            .unwrap();
        writer.copy_from_slice(indices);
        device.release_mapping_writer(writer);
    }

    let indices = buffer;

    Mesh {
        vertices,
        indices,
        index_count,
    }
}

fn shift_for_alignment<T>(alignment: T, offset: T) -> T
where
    T: From<u8> + Add<Output = T> + Sub<Output = T> + BitOr<Output = T> + PartialOrd,
{
    if offset > 0.into() && alignment > 0.into() {
        ((offset - 1.into()) | (alignment - 1.into())) + 1.into()
    } else {
        offset
    }
}

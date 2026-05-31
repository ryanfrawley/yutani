pub mod blur;
pub mod camera;
pub mod glow;
pub mod images;
pub mod texture;
pub mod vertex;

use wgpu::util::DeviceExt;

/// Create a `UNIFORM | COPY_DST` buffer initialized to a single `value`.
///
/// Every post-processing pass (blur, glow) seeds its uniform block with one
/// zero/default-valued struct and then `queue.write_buffer`s updates each
/// frame. The `bytemuck::cast_slice(&[value])` + usage-flag incantation is
/// invariant across all of them, so it lives here once rather than being
/// re-spelled at every `create_buffer_init` call site.
pub fn uniform_buffer<T: bytemuck::Pod>(
    device: &wgpu::Device,
    label: &str,
    value: T,
) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(&[value]),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    })
}

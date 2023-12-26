#[rustfmt::skip]
pub const OPENGL_TO_WGPU_MATRIX: cgmath::Matrix4<f32> = cgmath::Matrix4::new(
    1.0, 0.0, 0.0, 0.0,
    0.0, 1.0, 0.0, 0.0,
    0.0, 0.0, 0.5, 0.5,
    0.0, 0.0, 0.0, 1.0,
);

pub struct Camera {
}

pub struct Projection {
    pub width: u32,
    pub height: u32,
}

impl Camera {
    pub fn build_view_projection_matrix(&self, width: f32, height: f32) -> cgmath::Matrix4<f32> {
        let view = cgmath::Matrix4::from_nonuniform_scale(1.0, 1.0, 1.0);
        let projection = cgmath::ortho(0.0, width, height, 0.0, -100.0, 100.0);
        OPENGL_TO_WGPU_MATRIX * view * projection
    }
}

pub struct CameraController {

}

// We need this for Rust to store our data correctly for the shaders
#[repr(C)]
// This is so we can store this in a buffer
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CameraUniform {
    // We can't use cgmath with bytemuck directly, so we'll have
    // to convert the Matrix4 into a 4x4 f32 array
    pub view_proj: [[f32; 4]; 4],
}

impl CameraUniform {
    pub fn new() -> Self {
        use cgmath::SquareMatrix;
        Self {
            view_proj: cgmath::Matrix4::identity().into(),
        }
    }

    pub fn update_view_proj(&mut self, camera: &Camera, width: f32, height: f32) {
        self.view_proj = camera.build_view_projection_matrix(width as f32, height as f32).into();
    }
}

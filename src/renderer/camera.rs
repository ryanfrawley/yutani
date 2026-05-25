pub const OPENGL_TO_WGPU_MATRIX: cgmath::Matrix4<f32> = cgmath::Matrix4::new(
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.5, 0.0, 0.0, 0.0, 1.0,
);

pub struct Camera {}

impl Camera {
    pub fn build_view_projection_matrix(&self, width: f32, height: f32) -> cgmath::Matrix4<f32> {
        self.build_view_projection_matrix_scrolled(width, height, 0.0)
    }

    /// Like `build_view_projection_matrix`, but folds a vertical pixel offset
    /// into the transform. `scroll_y` is applied in pixel space *before* the
    /// projection (rightmost factor), so it's equivalent to having added
    /// `scroll_y` to every vertex's Y — letting the renderer build the grid
    /// once at rest and slide it via a cheap uniform write instead of
    /// rebuilding all geometry each animation frame. See `state_render`.
    pub fn build_view_projection_matrix_scrolled(
        &self,
        width: f32,
        height: f32,
        scroll_y: f32,
    ) -> cgmath::Matrix4<f32> {
        let view = cgmath::Matrix4::from_nonuniform_scale(1.0, 1.0, -1.0);
        let projection = cgmath::ortho(0.0, width, height, 0.0, -100.0, 100.0);
        let translate = cgmath::Matrix4::from_translation(cgmath::vec3(0.0, scroll_y, 0.0));
        OPENGL_TO_WGPU_MATRIX * view * projection * translate
    }
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
        self.view_proj = camera
            .build_view_projection_matrix(width as f32, height as f32)
            .into();
    }

    /// Refresh the matrix with a vertical scroll offset folded in (see
    /// `Camera::build_view_projection_matrix_scrolled`).
    pub fn update_view_proj_scrolled(
        &mut self,
        camera: &Camera,
        width: f32,
        height: f32,
        scroll_y: f32,
    ) {
        self.view_proj = camera
            .build_view_projection_matrix_scrolled(width, height, scroll_y)
            .into();
    }
}

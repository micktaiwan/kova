use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;

pub fn create_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    pixel_format: MTLPixelFormat,
) -> Retained<ProtocolObject<dyn MTLRenderPipelineState>> {
    let shader_source = include_str!("../../shaders/terminal.metal");
    let source = NSString::from_str(shader_source);

    let library = device
        .newLibraryWithSource_options_error(&source, None)
        .expect("failed to compile Metal shaders");

    let vertex_fn_name = NSString::from_str("vertex_main");
    let fragment_fn_name = NSString::from_str("fragment_main");

    let vertex_fn = library
        .newFunctionWithName(&vertex_fn_name)
        .expect("vertex_main not found");
    let fragment_fn = library
        .newFunctionWithName(&fragment_fn_name)
        .expect("fragment_main not found");

    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(&vertex_fn));
    desc.setFragmentFunction(Some(&fragment_fn));

    let color_attachment = unsafe {
        desc.colorAttachments().objectAtIndexedSubscript(0)
    };
    color_attachment.setPixelFormat(pixel_format);

    // Enable alpha blending for text
    color_attachment.setBlendingEnabled(true);
    color_attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    color_attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    color_attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&desc)
        .expect("failed to create render pipeline state")
}

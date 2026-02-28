#include <metal_stdlib>
using namespace metal;

struct VertexIn {
    float2 position   [[attribute(0)]];
    float2 tex_coords [[attribute(1)]];
    float4 color      [[attribute(2)]];
    float4 bg_color   [[attribute(3)]];
};

struct VertexOut {
    float4 position [[position]];
    float2 tex_coords;
    float4 color;
    float4 bg_color;
};

struct Vertex {
    float2 position;
    float2 tex_coords;
    float4 color;
    float4 bg_color;
};

vertex VertexOut vertex_main(
    uint vid [[vertex_id]],
    const device Vertex* vertices [[buffer(0)]],
    const device float2& viewport_size [[buffer(1)]],
    const device float2& atlas_size [[buffer(2)]]
) {
    VertexOut out;
    Vertex v = vertices[vid];

    // Convert pixel coords to NDC: (0,0) top-left, (w,h) bottom-right
    float2 ndc;
    ndc.x = (v.position.x / viewport_size.x) * 2.0 - 1.0;
    ndc.y = 1.0 - (v.position.y / viewport_size.y) * 2.0;

    out.position = float4(ndc, 0.0, 1.0);
    out.tex_coords = v.tex_coords;
    out.color = v.color;
    out.bg_color = v.bg_color;

    return out;
}

fragment float4 fragment_main(
    VertexOut in [[stage_in]],
    texture2d<float> atlas [[texture(0)]]
) {
    // If bg_color alpha > 0, this is a background quad
    if (in.bg_color.a > 0.0) {
        return in.bg_color;
    }

    constexpr sampler s(mag_filter::linear, min_filter::linear);
    float4 tex_color = atlas.sample(s, in.tex_coords);

    // Color emoji: color.a == 2.0 signals color glyph — use texture directly
    if (in.color.a > 1.5) {
        return tex_color;
    }

    // Grayscale glyph: use luminance as alpha mask
    float alpha = max(max(tex_color.r, tex_color.g), tex_color.b);

    return float4(in.color.rgb, in.color.a * alpha);
}

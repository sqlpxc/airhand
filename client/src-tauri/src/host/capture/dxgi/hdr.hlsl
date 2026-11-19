// DXGI FP16 desktop textures are linear scRGB with sRGB primaries.
Texture2D<float4> desktop : register(t0);
cbuffer Display : register(b0) { float inverseSdrWhite; };

float4 vertex(uint id : SV_VertexID) : SV_Position {
    // One clockwise triangle covers the viewport, with no vertex buffer.
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}

float4 pixel(float4 position : SV_Position) : SV_Target {
    float3 rgb = max(0, desktop.Load(int3(position.xy, 0)).rgb * inverseSdrWhite);
    // Preserve shadows and midtones. Roll highlights smoothly towards SDR white,
    // applying the same gain to all three channels to preserve their ratios.
    float peak = max(rgb.r, max(rgb.g, rgb.b));
    if (peak > 0.75) {
        float mapped = 1.0 - 0.0625 / (peak - 0.5);
        rgb *= mapped / peak;
    }
    float3 srgb = lerp(12.92 * rgb, 1.055 * pow(rgb, 1.0 / 2.4) - 0.055, step(0.0031308, rgb));
    return float4(srgb, 1);
}

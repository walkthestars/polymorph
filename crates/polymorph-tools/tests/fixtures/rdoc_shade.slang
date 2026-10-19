// Minimal slang triangle shaders for the N2a compile smoke (chunk 114).
// Plain entry functions (compiled with explicit -entry/-stage — no
// attribute-magic dependence). Distinct constants per vertex so the
// compile is real work, not an empty translation unit. Matches the
// RenderDoc triangle fixture geometry (rdoc_triangle.c).
float4 vsMain(float2 pos : POSITION, float3 col : COLOR, out float3 vcol : TEXCOORD0) : SV_Position
{
    vcol = col;
    return float4(pos, 0.0, 1.0);
}

float4 fsMain(float3 vcol : TEXCOORD0) : SV_Target
{
    return float4(vcol, 1.0);
}

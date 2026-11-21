// Output encodings shared by the blit and the overlay.
const int OUT_SAME = 0;      // the swapchain takes the content's encoding as is
const int OUT_PQ_TO_SDR = 1; // PQ BT.2020 content, SDR BT.709 swapchain: tone-map
const int OUT_SDR_TO_PQ = 2; // SDR content, HDR10 swapchain
const int OUT_LINEAR = 3;    // SDR content, an _SRGB swapchain (it encodes on write)

vec3 pq_eotf(vec3 e) { // PQ to cd/m²
    const float m1 = 0.1593017578125, m2 = 78.84375, c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
    vec3 ep = pow(max(e, 0.0), vec3(1.0 / m2));
    return 10000.0 * pow(max(ep - c1, 0.0) / (c2 - c3 * ep), vec3(1.0 / m1));
}

vec3 pq_oetf(vec3 nits) {
    const float m1 = 0.1593017578125, m2 = 78.84375, c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
    vec3 y = pow(clamp(nits / 10000.0, 0.0, 1.0), vec3(m1));
    return pow((c1 + c2 * y) / (1.0 + c3 * y), vec3(m2));
}

// Column-major, as GLSL reads them.
const mat3 BT2020_TO_709 = mat3( 1.6605, -0.1246, -0.0182,
                                -0.5876,  1.1329, -0.1006,
                                -0.0728, -0.0083,  1.1187);
const mat3 BT709_TO_2020 = mat3(0.6274, 0.0691, 0.0164,
                                0.3293, 0.9195, 0.0880,
                                0.0433, 0.0114, 0.8956);

vec3 encode_out(vec3 c, int mode, float sdr_nits, float peak_nits) {
    if (mode == OUT_PQ_TO_SDR) {
        vec3 lin = BT2020_TO_709 * pq_eotf(c) / sdr_nits; // 1.0 is SDR white
        float l = max(dot(lin, vec3(0.2126, 0.7152, 0.0722)), 1e-5);
        float w = max(peak_nits / sdr_nits, 1.0);
        float lm = l * (1.0 + l / (w * w)) / (1.0 + l); // extended Reinhard on luminance
        lin = max(lin * (lm / l), 0.0);
        return pow(clamp(lin, 0.0, 1.0), vec3(1.0 / 2.2));
    }
    if (mode == OUT_SDR_TO_PQ) {
        vec3 lin = pow(clamp(c, 0.0, 1.0), vec3(2.2));
        return pq_oetf(BT709_TO_2020 * lin * sdr_nits);
    }
    if (mode == OUT_LINEAR) {
        return pow(clamp(c, 0.0, 1.0), vec3(2.2));
    }
    return c;
}

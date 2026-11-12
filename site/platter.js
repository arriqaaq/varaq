// The hero, ray-traced in one fragment shader: a hard drive, then an NVMe SSD.
// Scroll floats over the platter and dives onto one track until it is a lane. The lane crawls at disk
// speed; then the disk stops, the lane rushes, and it becomes a row of flash pages on an M.2 SSD.
// The camera pulls out to show the whole drive, reading pages across its chips at once.

const VERT = 'attribute vec2 p; void main(){ gl_Position = vec4(p, 0.0, 1.0); }';
const FRAG = `
precision highp float;
uniform vec2 uRes; uniform float uTime, uRot, uWarp, uArmA, uFlow, uHi, uScene, uRead, uHot;
uniform vec3 uCam, uTgt, uBT, uBC; uniform vec2 uLight;
const float PI = 3.14159265, TAU = 6.2831853;
const float RT = 0.621;         // centre of the track the camera dives onto (track 172)
const float PITCH = 0.0036;     // track pitch, and the pitch of the flash page rows
const float BIT = 0.00085;      // bit cell along a track
const float HA = 0.045;         // the arm, above the platter
const float HB = -0.05;         // the drive's base plate, below it
const vec2 PIV = vec2(1.16, -0.80);
const vec2 HEAD0 = vec2(0.585, -0.205);
const vec3 cA = vec3(0.72, 0.76, 1.0), cB = vec3(0.64, 0.91, 0.88);

float hash(vec2 p){ return fract(sin(dot(p, vec2(41.3, 289.1))) * 43758.5453); }
vec2 rot2(vec2 v, float a){ float c = cos(a), s = sin(a); return vec2(c * v.x - s * v.y, s * v.x + c * v.y); }
float sdBox(vec2 p, vec2 b, float r){ vec2 q = abs(p) - b + r; return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r; }
vec3 tone(vec3 c){ c = c / (1.0 + c); return pow(c, vec3(1.0 / 2.2)); }

vec3 env(vec3 d){
  vec3 c = mix(vec3(0.0004), vec3(0.0035, 0.0036, 0.0042), smoothstep(-0.2, 1.0, d.y));
  vec3 k = normalize(vec3(uLight.x, 0.9, uLight.y));
  c += vec3(1.0, 0.99, 0.97) * smoothstep(0.972, 0.992, dot(d, k)) * 3.0;
  c += vec3(0.86, 0.89, 1.0) * smoothstep(0.022, 0.006, abs(d.x - 0.55 * d.y + 0.12)) * smoothstep(0.05, 0.4, d.y) * smoothstep(0.98, 0.7, d.y) * 1.1;
  c += cA * 0.22 * smoothstep(0.93, 0.985, dot(d, normalize(vec3(-0.7, 0.35, -0.6))));
  c += cB * 0.16 * smoothstep(0.94, 0.99, dot(d, normalize(vec3(0.8, 0.25, -0.5))));
  return c;
}

/* ---------------- the hard drive ---------------- */
float sdArm(vec2 p){
  vec2 head = PIV + rot2(HEAD0 - PIV, uArmA);
  vec2 ba = head - PIV; float L = length(ba); vec2 u = ba / L;
  vec2 q = p - PIV; float t = clamp(dot(q, u) / L, -0.30, 1.0);
  float w = t < 0.0 ? mix(0.072, 0.135, -t / 0.30) : mix(0.072, 0.014, pow(t, 0.75));
  float d = length(q - u * L * t) - w;
  d = min(d, length(q) - 0.105);
  vec2 hq = p - head; vec2 hl = vec2(dot(hq, u), dot(hq, vec2(-u.y, u.x)));
  d = min(d, sdBox(hl - vec2(0.006, 0.0), vec2(0.022, 0.017), 0.004));
  return d;
}
vec3 armShade(vec2 p, vec3 rd){
  vec2 e = vec2(0.004, 0.0);
  float d = sdArm(p);
  vec2 g = vec2(sdArm(p + e.xy) - sdArm(p - e.xy), sdArm(p + e.yx) - sdArm(p - e.yx)) / (2.0 * e.x);
  float bev = smoothstep(-0.016, 0.0, d);
  vec3 n = normalize(vec3(0.0, 1.0, 0.0) + vec3(g.x, 0.0, g.y) * bev * 1.6);
  vec3 V = -rd, R = reflect(rd, n);
  vec2 head = PIV + rot2(HEAD0 - PIV, uArmA); vec2 u = normalize(head - PIV);
  vec3 T = vec3(u.x, 0.0, u.y);
  vec3 L = normalize(vec3(uLight.x, 1.1, uLight.y)); vec3 H = normalize(L + V);
  float tdh = dot(T, H); float an = pow(sqrt(max(0.0, 1.0 - tdh * tdh)), 90.0);
  float fres = 0.12 + 0.88 * pow(1.0 - max(dot(n, V), 0.0), 5.0);
  float rr = length(p - PIV);
  float ring = smoothstep(0.004, 0.0, abs(rr - 0.07)) + smoothstep(0.003, 0.0, abs(rr - 0.03));
  return vec3(0.022, 0.022, 0.025) * (0.3 + max(dot(n, L), 0.0)) + env(R) * fres * 0.5 + vec3(0.9, 0.92, 1.0) * an * 0.3 + ring * 0.03 + vec3(1.0) * bev * 0.02;
}
// the base plate: a brushed casing, its rim, the well the platter sits in, the voice-coil magnet, screws
vec3 baseShade(vec2 p, vec3 rd, out float m){
  float d = sdBox(p - vec2(0.17, -0.12), vec2(1.36, 1.44), 0.14);
  m = step(d, 0.0);
  vec3 V = -rd; vec3 L = normalize(vec3(uLight.x, 1.1, uLight.y));
  float rim = smoothstep(-0.075, -0.06, d);
  float wellR = length(p);
  float well = smoothstep(1.05, 1.04, wellR);
  float wellEdge = smoothstep(0.006, 0.0, abs(wellR - 1.05));
  float mag = sdBox(rot2(p - vec2(1.22, -1.12), 0.5), vec2(0.24, 0.15), 0.06);
  vec3 n = normalize(vec3(0.0, 1.0, 0.0) + vec3(p.x, 0.0, p.y) / max(wellR, 1e-3) * wellEdge * 0.9);
  float magEdge = smoothstep(0.012, 0.0, abs(mag));
  vec3 base = vec3(0.005, 0.005, 0.006) * (1.0 - 0.3 * well);
  base = mix(base, vec3(0.009, 0.009, 0.011), step(mag, 0.0));
  base += vec3(0.02) * rim * (1.0 - smoothstep(-0.02, 0.0, d));
  float scr = 0.0;
  scr = max(scr, smoothstep(0.03, 0.026, length(p - vec2(-1.08, -1.45))));
  scr = max(scr, smoothstep(0.03, 0.026, length(p - vec2(1.42, -1.45))));
  scr = max(scr, smoothstep(0.03, 0.026, length(p - vec2(-1.08, 1.2))));
  scr = max(scr, smoothstep(0.03, 0.026, length(p - vec2(1.42, 1.2))));
  scr = max(scr, smoothstep(0.03, 0.026, length(p - vec2(1.42, 0.1))));
  scr = max(scr, smoothstep(0.03, 0.026, length(p - vec2(-1.08, -0.1))));
  vec3 R = reflect(rd, n); vec3 H = normalize(L + V);
  float hx = dot(vec3(1.0, 0.0, 0.0), H);
  float brush = pow(sqrt(max(0.0, 1.0 - hx * hx)), 60.0);
  return base * (0.3 + max(dot(n, L), 0.0)) + env(R) * 0.025 + vec3(0.8, 0.82, 0.9) * brush * 0.03 * (1.0 - well) + (wellEdge + magEdge) * 0.03 + scr * 0.04;
}
vec3 hddScene(vec3 ro, vec3 rd, float pxs, vec3 bg){
  vec3 outc = bg;
  float armA = 0.0; vec3 armC = vec3(0.0); float tArm = 1e9;
  if (rd.y < -1e-4 && ro.y > HA) {
    tArm = (HA - ro.y) / rd.y;
    vec3 Pa = ro + rd * tArm;
    float fpa = tArm * pxs / max(abs(rd.y), 0.05);
    armA = smoothstep(fpa, -fpa, sdArm(Pa.xz));
    if (armA > 0.0) armC = armShade(Pa.xz, rd);
  }
  if (rd.y > -1e-5) return outc;
  float t = -ro.y / rd.y;
  vec3 P = ro + rd * t;
  float a = P.x, b = P.z, r = length(vec2(a, b));
  float fp = t * pxs / max(abs(rd.y), 0.025);
  float cover = smoothstep(1.0 + fp, 1.0 - fp, r) * smoothstep(0.024 - fp, 0.024 + fp, r);
  vec3 L = normalize(vec3(uLight.x, 1.1, uLight.y));
  if (cover > 0.0) {
    vec3 Nf = vec3(0.0, 1.0, 0.0);
    vec3 Er = vec3(a, 0.0, b) / max(r, 1e-4);
    vec3 Et = cross(Nf, Er);
    float th = atan(b, a) + uRot;
    vec3 Vv = -rd;
    vec3 n = Nf; float ao = 1.0; float aniso = 0.0; float F0 = 0.06;
    vec3 diff = vec3(0.006, 0.006, 0.007);
    vec3 emit = vec3(0.0);
    if (r < 0.2) {
      diff = vec3(0.012, 0.012, 0.014); F0 = 0.2;
      n = normalize(Nf - Er * (smoothstep(0.004, 0.0, abs(r - 0.152)) * 0.9 - smoothstep(0.004, 0.0, abs(r - 0.197)) * 0.7 + smoothstep(0.005, 0.0, abs(r - 0.045)) * 0.8));
      float cell = floor(th * 6.0 / TAU + 0.5);
      float ang = cell * TAU / 6.0 - uRot;
      vec2 sc = 0.098 * vec2(cos(ang), sin(ang));
      float ds = length(vec2(a, b) - sc);
      if (ds < 0.017) { diff = vec3(0.02); ao = 0.6; n = normalize(Nf + Er * smoothstep(0.012, 0.017, ds) * 0.6); }
      aniso = 0.5;
    } else if (r < 0.236) {
      diff = vec3(0.005); aniso = 0.2;
    } else if (r < 0.975) {
      float ph = r / PITCH; float g = fract(ph); float trk = floor(ph);
      float x = g - 0.5;
      float resT = smoothstep(0.7, 0.18, fp / PITCH);
      float guard = smoothstep(0.06, 0.0, abs(abs(x) - 0.5));
      n = normalize(Nf - Er * sign(x) * guard * 0.55 * resT);
      float s = th * r;
      float resB = smoothstep(0.8, 0.2, fp / BIT);
      float cell = floor(s / BIT);
      float pol = step(0.5, hash(vec2(cell, trk)));
      float pol0 = step(0.5, hash(vec2(cell - 1.0, trk)));
      float edge = (pol != pol0 ? 1.0 : 0.0) * smoothstep(0.12, 0.0, fract(s / BIT));
      diff = mix(vec3(0.006, 0.006, 0.007), vec3(0.013, 0.0135, 0.016), pol * resB * resT);
      float wedge = fract(th * 96.0 / TAU);
      float servo = smoothstep(0.035, 0.03, wedge) * (0.5 + 0.5 * step(0.5, fract(trk * 0.5)));
      diff += vec3(0.014, 0.015, 0.018) * servo * (0.35 + 0.65 * resT);
      emit += vec3(0.10, 0.105, 0.12) * edge * resB * resT * (1.0 - guard);
      aniso = 1.0 - resT * 0.95;
      float lane = trk == floor(RT / PITCH) ? 1.0 : 0.0;
      float flowS = s / 0.0042 + uFlow;
      float dash = smoothstep(0.42, 0.5, fract(flowS)) * smoothstep(0.98, 0.9, fract(flowS));
      vec3 grad = mix(cA, cB, 0.5 + 0.5 * sin(s * 40.0 + uFlow * 0.2));
      float lit = max(uWarp, uHi * 0.5);
      emit += grad * guard * lane * lit * 2.6;
      emit += vec3(1.0) * dash * smoothstep(0.05, 0.0, abs(x)) * lane * lit * 1.8;
    } else {
      diff = vec3(0.006); aniso = 0.25;
    }
    n = normalize(n + Er * smoothstep(0.985, 1.0, r) * 1.6);
    float ndv = max(dot(n, Vv), 0.0);
    float fres = F0 + (1.0 - F0) * pow(1.0 - ndv, 5.0);
    vec3 R = reflect(-Vv, n);
    vec3 H = normalize(L + Vv);
    vec3 refl = env(R);
    if (R.y > 1e-4) {
      float tr = (HA - P.y) / R.y; vec3 Q = P + R * tr;
      float m = smoothstep(0.004, -0.004, sdArm(Q.xz));
      if (m > 0.0) refl = mix(refl, armShade(Q.xz, R), m);
    }
    vec3 spec = refl * fres + vec3(pow(max(dot(n, H), 0.0), 500.0) * 3.0) * fres;
    float tdh = dot(Et, H); float kk = pow(sqrt(max(0.0, 1.0 - tdh * tdh)), 420.0);
    float tdh2 = dot(Et, normalize(normalize(vec3(-0.6, 0.6, 0.4)) + Vv)); float kk2 = pow(sqrt(max(0.0, 1.0 - tdh2 * tdh2)), 160.0);
    spec += aniso * (vec3(1.0, 0.98, 0.95) * kk * 0.55 + vec3(0.75, 0.8, 1.0) * kk2 * 0.18) * (0.2 + fres);
    vec3 dif = diff * (0.2 + max(dot(n, L), 0.0));
    vec3 Ps = P + L * (HA - P.y) / L.y;
    float sh = mix(1.0, smoothstep(-0.01, 0.035, sdArm(Ps.xz)), 0.85);
    vec3 col = (dif + spec * mix(0.35, 1.0, sh)) * ao * mix(0.4, 1.0, sh) + emit;
    outc = mix(outc, tone(col), cover);
  }
  if (armA > 0.0 && tArm < t) outc = mix(outc, tone(armC), armA);
  return outc;
}

/* ---------------- the NVMe SSD: an M.2 stick, flash pages on its chips ---------------- */
vec3 ssdScene(vec3 ro, vec3 rd, float pxs, vec3 bg){
  vec3 N = vec3(0.0, 1.0, 0.0);
  vec3 BN = cross(N, uBT);
  float den = dot(rd, N);
  if (den > -1e-5) return bg;
  float t = dot(uBC - ro, N) / den;
  if (t <= 0.0) return bg;
  vec3 P = ro + rd * t;
  float u = dot(P - uBC, uBT), v = dot(P - uBC, BN);
  float fp = t * pxs / max(abs(den), 0.025);
  float bd = sdBox(vec2(u, v), vec2(0.8, 0.22), 0.025);
  bd = max(bd, -(length(vec2(u + 0.8, v)) - 0.032));
  bd = max(bd, -sdBox(vec2(u - 0.8, v + 0.07), vec2(0.06, 0.012), 0.004));
  float cover = smoothstep(fp, -fp, bd);
  // the board's shadow on the dark below it
  vec3 below = bg;
  if (cover <= 0.0) return below;
  vec3 Vv = -rd; vec3 L = normalize(vec3(uLight.x, 1.1, uLight.y)); vec3 H = normalize(L + Vv);
  vec3 n = N; vec3 diff = vec3(0.007, 0.007, 0.008); float F0 = 0.035; float gloss = 0.5; vec3 emit = vec3(0.0);
  float aniso = 0.0;
  float tr = smoothstep(0.0016, 0.0, abs(fract(v / 0.022) - 0.5) * 0.022) * step(abs(v), 0.19);
  diff += vec3(0.006) * tr;
  // the read streams: a pulse per core, along the traces from the controller to the flash
  float lanei = floor(v / 0.022 + 0.5);
  float pulse = fract(u * 3.0 - uTime * (1.2 + hash(vec2(lanei, 3.0)) * 0.8) - hash(vec2(lanei, 9.0)));
  float stream = tr * smoothstep(0.9, 1.0, pulse) * step(-0.42, u) * step(u, 0.62) * step(abs(lanei), 7.0);
  emit += mix(cA, cB, hash(vec2(lanei, 1.0))) * stream * uRead * 1.8;
  if (u > 0.66) {
    float f = smoothstep(0.35, 0.3, abs(fract(v / 0.016) - 0.5));
    diff = mix(diff, vec3(0.16, 0.165, 0.18), f * step(abs(v), 0.2)); F0 = mix(F0, 0.7, f); gloss = 1.0; aniso = f;
  }
  float ctl = sdBox(vec2(u + 0.54, v), vec2(0.12, 0.12), 0.01);
  float dram = sdBox(vec2(u + 0.30, v), vec2(0.05, 0.09), 0.006);
  float n1 = sdBox(vec2(u + 0.02, v), vec2(0.16, 0.165), 0.008);
  float n2 = sdBox(vec2(u - 0.36, v), vec2(0.16, 0.165), 0.008);
  float chip = min(min(ctl, dram), min(n1, n2));
  if (chip < 0.0) {
    diff = vec3(0.0022, 0.0022, 0.0026); F0 = 0.04; gloss = 0.45;
    emit += vec3(0.06, 0.062, 0.07) * smoothstep(0.004, 0.0, abs(chip + 0.0015));
    if (min(n1, n2) < 0.0) {
      // flash pages: rows at the platter's track pitch, each row a run of pages
      float pv = PITCH, pu = 0.0084;
      float cu = u - 0.36;
      float row = floor(cu / pv + 0.5), col = floor(v / pu);
      float gv = abs(fract(cu / pv + 0.5) - 0.5), gu = fract(v / pu);
      float res = smoothstep(0.7, 0.15, fp / pv);
      float line = max(smoothstep(0.06, 0.0, abs(gv - 0.5)), smoothstep(0.04, 0.0, min(gu, 1.0 - gu)));
      emit += vec3(0.05, 0.052, 0.06) * line * res;
      // planes and blocks: coarser structure visible from further out
      float blk = max(smoothstep(0.0022, 0.0, 0.04 - abs(fract(u / 0.08) - 0.5) * 0.08), smoothstep(0.0022, 0.0, 0.04125 - abs(fract(v / 0.0825) - 0.5) * 0.0825));
      emit += vec3(0.03, 0.031, 0.036) * blk * (1.0 - res * 0.5);
      // pages lit by reads, many at once
      float ph = uTime * 6.0 + hash(vec2(col, row)) * 6.0;
      float slot = floor(ph);
      float on = step(1.0 - 0.07 * uRead - 0.025 * uWarp, hash(vec2(col, row) + slot * 1.37));
      float fade = 1.0 - fract(ph);
      vec3 lc = mix(cA, cB, hash(vec2(row, col * 0.1)));
      float cellIn = smoothstep(0.5, 0.38, gv) * smoothstep(0.0, 0.08, gu) * smoothstep(1.0, 0.92, gu);
      emit += lc * on * fade * cellIn * res * 1.4;
      // unresolved from far away: whole blocks flicker as their pages are read
      vec2 bk = floor(vec2(u / 0.08, v / 0.0825));
      float bph = uTime * 3.0 + hash(bk) * 3.0;
      float bon = step(1.0 - 0.35 * uRead, hash(bk + floor(bph) * 1.91)) * (1.0 - fract(bph));
      emit += mix(cA, cB, hash(bk + 4.0)) * bon * (1.0 - res) * 0.45 * smoothstep(-0.004, -0.01, min(n1, n2));
      // the lane the camera arrived in, continued from the disk
      float lane = row == 0.0 && n2 < 0.0 ? 1.0 : 0.0;
      float flowS = v / 0.0042 + uFlow;
      float dash = smoothstep(0.42, 0.5, fract(flowS)) * smoothstep(0.98, 0.9, fract(flowS));
      emit += mix(cA, cB, 0.5) * smoothstep(0.06, 0.0, abs(gv - 0.5)) * lane * uWarp * 2.6;
      emit += vec3(1.0) * dash * smoothstep(0.1, 0.0, gv) * lane * uWarp * 1.6;
      // the pages you use most: held in memory, lit steady
      emit += vec3(1.0) * uHot * step(0.985, hash(vec2(col * 0.37, row * 0.11))) * cellIn * res * 0.9;
      emit += vec3(0.9, 0.92, 1.0) * uHot * step(0.93, hash(bk * 1.7 + 2.0)) * (1.0 - res) * 0.35 * smoothstep(-0.004, -0.01, min(n1, n2));
    }
    if (ctl < 0.0) { float pin = smoothstep(0.3, 0.2, abs(fract(u / 0.012) - 0.5)) * smoothstep(-0.02, -0.01, ctl); emit += vec3(0.03) * pin + mix(cA, cB, 0.5) * uRead * 0.05 * smoothstep(-0.03, -0.06, ctl); }
    if (dram < 0.0) emit += vec3(0.9, 0.92, 1.0) * uHot * 0.08;
  }
  float ndv = max(dot(n, Vv), 0.0);
  float fres = F0 + (1.0 - F0) * pow(1.0 - ndv, 5.0);
  vec3 R = reflect(-Vv, n);
  vec3 spec = (env(R) * fres * gloss + vec3(pow(max(dot(n, H), 0.0), 300.0) * 2.0) * fres * gloss) * 0.35;
  float tdh = dot(BN, H); spec += aniso * vec3(0.9, 0.92, 1.0) * pow(sqrt(max(0.0, 1.0 - tdh * tdh)), 80.0) * 0.4;
  vec3 col = diff * (0.2 + max(dot(n, L), 0.0)) + spec + emit;
  col += vec3(0.08) * smoothstep(-0.006, 0.0, bd);
  return mix(below, tone(col), cover);
}

void main(){
  vec2 uv = (gl_FragCoord.xy - 0.5 * uRes) / uRes.y;
  vec3 f = normalize(uTgt - uCam), r0 = normalize(cross(f, vec3(0.0, 1.0, 0.0))), up = cross(r0, f);
  vec3 rd = normalize(f * 1.85 + uv.x * r0 + uv.y * up);
  vec3 bg = vec3(0.0235, 0.0235, 0.027) * (1.0 - 0.3 * dot(uv, uv));
  bg += vec3(0.9, 0.92, 1.0) * 0.01 * smoothstep(0.4, 0.0, abs(uv.x * 0.8 + uv.y * 0.45 - 0.3));
  float pxs = 1.0 / (1.85 * uRes.y);
  vec3 c;
  if (uScene <= 0.001) c = hddScene(uCam, rd, pxs, bg);
  else if (uScene >= 0.999) c = ssdScene(uCam, rd, pxs, bg);
  else c = mix(hddScene(uCam, rd, pxs, bg), ssdScene(uCam, rd, pxs, bg), uScene);
  c += vec3(0.85, 0.88, 1.0) * uWarp * uWarp * 0.03 * smoothstep(0.5, 0.0, length(uv));
  c += (hash(gl_FragCoord.xy + uTime) - 0.5) * 0.006;
  gl_FragColor = vec4(c, 1.0);
}`;

/* ---------- the camera ---------- */
const v3 = {
  add: (a, b) => [a[0] + b[0], a[1] + b[1], a[2] + b[2]],
  mul: (a, s) => [a[0] * s, a[1] * s, a[2] * s],
  mix: (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t],
  norm: a => { const l = Math.hypot(a[0], a[1], a[2]) || 1; return [a[0] / l, a[1] / l, a[2] / l]; },
};
const clamp01 = x => Math.min(1, Math.max(0, x));
const smooth = (a, b, x) => { if (b <= a) return x >= a ? 1 : 0; const t = clamp01((x - a) / (b - a)); return t * t * (3 - 2 * t); };
const ease = t => t < .5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;

const RT = (Math.floor(0.62 / 0.0036) + 0.5) * 0.0036, ANG = 44 * Math.PI / 180;
const DIVE = [RT * Math.cos(ANG), 0, RT * Math.sin(ANG)];
const TAN = [-Math.sin(ANG), 0, Math.cos(ANG)];
const BN = [Math.cos(ANG), 0, Math.sin(ANG)];
const BT = v3.mul(BN, -1);                              // the stick's long axis: across the lane, left to right on screen
const BC = v3.add(DIVE, v3.mul(BT, -0.36));            // the lane runs over its second flash chip
const T0 = [0, 0, 0.0], D0 = 3.72, DIR0 = v3.norm([0.0, 1, 0.8]);
const DIR1 = v3.norm(v3.add(v3.mul(TAN, -1), [0, 0.085, 0]));
const D1 = 0.0115;
const DS = 2.7, DIRS = v3.norm(v3.add([0, 1, 0], v3.mul(TAN, -0.004)));
const TS = v3.add(BC, v3.mul(TAN, 0.0));

// phases of the hero's scroll progress s (0..1)
export const PH = {
  float1: .08,                   // floating over the drive
  dive0: .08, dive1: .32,        // down onto one track
  lane1: .40,                    // in the lane, at disk speed
  warp0: .40, morph0: .44, morph1: .49,   // the disk stops; the lane rushes and becomes flash
  rise0: .53, rise1: .64,        // out of the flash: the whole SSD
  hot0: .72, hot1: .82,          // the pages you use, held in memory
  end: .84,
};

function cameraAt(s, t, mx, my, W, H) {
  let tgt, dist, dir;
  if (s < PH.dive1) {
    const e = ease(smooth(PH.dive0, PH.dive1, s));
    tgt = v3.mix(T0, DIVE, smooth(0, .5, e));
    dist = Math.exp(Math.log(D0) + (Math.log(D1) - Math.log(D0)) * e);
    dir = v3.norm(v3.mix(DIR0, DIR1, Math.pow(e, 2.4)));
  } else if (s < PH.rise0) {
    tgt = DIVE; dist = D1; dir = DIR1;
  } else {
    const e = ease(smooth(PH.rise0, PH.rise1, s));
    const side = ease(smooth(PH.end, PH.end + .04, s));
    tgt = v3.add(v3.mix(DIVE, TS, smooth(.05, .7, e)), v3.mul(BT, .5 * side));
    dist = Math.exp(Math.log(D1) + (Math.log(DS) - Math.log(D1)) * e) * (1 + .22 * side);
    dir = v3.norm(v3.mix(DIR1, DIRS, smooth(0, .18, e)));
  }
  const o = Math.min(1, dist / 1.5) * (1 - smooth(PH.warp0, PH.morph0, s));
  const ay = mx * .16 * o, ax = -my * .08 * o;
  const cy = Math.cos(ay), sy = Math.sin(ay);
  dir = [dir[0] * cy + dir[2] * sy, dir[1], -dir[0] * sy + dir[2] * cy];
  dir = v3.norm([dir[0], dir[1] + ax, dir[2]]);
  const aspect = W / Math.max(1, H), fit = Math.max(1, 1.45 / aspect);
  const d2 = dist * (1 + (fit - 1) * clamp01(dist / 3));
  return { cam: v3.add(tgt, v3.mul(dir, d2)), tgt, dist };
}

export function initPlatter(canvas, opts = {}) {
  const gl = canvas.getContext('webgl', { antialias: false, alpha: false, preserveDrawingBuffer: !!opts.preserve });
  if (!gl) return null;
  const sh = (type, src) => { const s = gl.createShader(type); gl.shaderSource(s, src); gl.compileShader(s); if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) { console.error(gl.getShaderInfoLog(s)); return null; } return s; };
  const vs = sh(gl.VERTEX_SHADER, VERT), fs = sh(gl.FRAGMENT_SHADER, FRAG);
  if (!vs || !fs) return null;
  const prog = gl.createProgram(); gl.attachShader(prog, vs); gl.attachShader(prog, fs); gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) return null;
  gl.useProgram(prog);
  const buf = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, buf);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
  const loc = gl.getAttribLocation(prog, 'p'); gl.enableVertexAttribArray(loc); gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
  const U = {};
  ['uRes', 'uTime', 'uRot', 'uWarp', 'uArmA', 'uFlow', 'uHi', 'uScene', 'uRead', 'uHot', 'uCam', 'uTgt', 'uBT', 'uBC', 'uLight'].forEach(k => U[k] = gl.getUniformLocation(prog, k));

  const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
  let W = 0, H = 0, dpr = 1;
  const state = { s: 0, spin: 1, rot: 0, omega: .55, flow: 0, mx: 0, my: 0, smx: 0, smy: 0, dragging: false, visible: true, cur: null, seek: 0, armA: 0, warp: 0, t: 0 };
  function resize() {
    W = innerWidth; H = innerHeight;
    dpr = Math.min(opts.dpr || 1.25, devicePixelRatio || 1);
    canvas.width = Math.round(W * dpr); canvas.height = Math.round(H * dpr);
    gl.viewport(0, 0, canvas.width, canvas.height);
  }
  resize(); addEventListener('resize', () => { clearTimeout(resize.t); resize.t = setTimeout(resize, 120); });

  let lastAng = 0, lastT = 0;
  const angOf = e => Math.atan2(e.clientY - H * .6, e.clientX - W / 2);
  canvas.addEventListener('pointerdown', e => { if (state.s > .03) return; state.dragging = true; lastAng = angOf(e); lastT = performance.now(); canvas.style.cursor = 'grabbing'; });
  addEventListener('pointerup', () => { state.dragging = false; canvas.style.cursor = ''; });
  addEventListener('pointermove', e => {
    state.mx = e.clientX / W * 2 - 1; state.my = e.clientY / H * 2 - 1;
    if (!state.dragging) return;
    const a = angOf(e); let d = a - lastAng; if (d > Math.PI) d -= Math.PI * 2; if (d < -Math.PI) d += Math.PI * 2;
    const now = performance.now(), dt = Math.max(.008, (now - lastT) / 1000);
    state.rot -= d; state.omega += (-d / dt - state.omega) * .5; lastAng = a; lastT = now;
  });

  const P = PH;
  let last = performance.now();
  function frame(now) {
    const dt = Math.min(.05, (now - last) / 1000); last = now; state.t += dt;
    const s = state.s;
    const crawl = smooth(P.dive1 - .02, P.dive1 + .02, s) * (1 - smooth(P.warp0, P.morph0, s));
    const rush = smooth(P.warp0, P.morph0 + .02, s) * (1 - smooth(P.morph1 + .01, P.rise0 + .03, s));
    const warp = Math.max(crawl * .35, rush);
    state.warp = warp; state.crawl = crawl; state.rush = rush;
    const scene = smooth(P.morph0 + .01, P.morph1, s);
    const read = smooth(P.morph1, P.rise1, s);
    const hot = 0;
    state.scene = scene; state.read = read; state.hot = hot;
    const base = reduce ? 0 : .55 * state.spin * (1 - scene);
    if (!state.dragging) state.omega += (base - state.omega) * Math.min(1, dt * 2.2);
    state.smx += (state.mx - state.smx) * Math.min(1, dt * 3); state.smy += (state.my - state.smy) * Math.min(1, dt * 3);
    const cur = cameraAt(s, state.t, state.smx, state.smy, W, H); state.cur = cur;
    const slow = Math.max(.004, Math.min(1, cur.dist / D0));
    state.rot += state.omega * dt * (s > .05 ? slow : 1);
    state.flow += dt * (crawl * .5 + rush * rush * 28);
    state.seek += dt;
    const seekA = reduce ? 0 : .045 * Math.sin(state.seek * .9) + .02 * Math.sign(Math.sin(state.seek * 2.3));
    state.armA += ((seekA * (1 - smooth(.05, .3, s))) - state.armA) * Math.min(1, dt * 6);
    if (state.visible) {
      gl.uniform2f(U.uRes, canvas.width, canvas.height);
      gl.uniform1f(U.uTime, (now % 100000) / 1000);
      gl.uniform1f(U.uRot, state.rot);
      gl.uniform1f(U.uWarp, warp);
      gl.uniform1f(U.uArmA, state.armA);
      gl.uniform1f(U.uFlow, state.flow);
      gl.uniform1f(U.uHi, smooth(.22, .32, s));
      gl.uniform1f(U.uScene, scene);
      gl.uniform1f(U.uRead, read);
      gl.uniform1f(U.uHot, hot);
      gl.uniform3fv(U.uCam, cur.cam); gl.uniform3fv(U.uTgt, cur.tgt);
      gl.uniform3fv(U.uBT, BT); gl.uniform3fv(U.uBC, BC);
      gl.uniform2f(U.uLight, -.55 + state.smx * .5, .3 + state.smy * .4);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    }
    opts.onFrame && opts.onFrame(state);
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
  return { state, D0 };
}

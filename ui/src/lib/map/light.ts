// SPDX-License-Identifier: AGPL-3.0-only
// The recall light: a WebGL2 layer over the map. Every pulse and
// glow is one instanced quad whose motion is computed in the shader from its
// start time and duration, so the CPU uploads once per event and only sets
// uniforms per frame. The loop runs only while something is lit.

export const MAX_INSTANCES = 1500;
const FLOATS = 14; // per instance, see the attribute layout below

export interface Pulse {
  x0: number; y0: number; x1: number; y1: number;
  start: number; duration: number; width: number; brightness: number;
  color: [number, number, number];
}
export interface Glow {
  x: number; y: number; start: number; attack: number; decay: number;
  intensity: number; radius: number; color: [number, number, number];
  /** 0 glow, 1 ring that grows, 2 cool (suppression) */
  kind: 0 | 1 | 2;
}

const VERT = `#version 300 es
in vec2 a_corner;
in vec4 a_ends;      // x0 y0 x1 y1 (graph space)
in vec4 a_time;      // start, duration/decay, attack, kind(-1 = pulse)
in vec2 a_shape;     // width/radius, brightness/intensity
in vec4 a_color;     // rgb, unused
uniform mat3 u_matrix; // graph -> pixels
uniform vec2 u_size;
uniform float u_now;
uniform float u_scale;
out vec2 v_uv;
out float v_alpha;
out vec3 v_color;
flat out float v_kind;
void main() {
  float start = a_time.x, dur = a_time.y, attack = a_time.z, kind = a_time.w;
  float t = u_now - start;
  vec2 p0 = (u_matrix * vec3(a_ends.xy, 1.0)).xy;
  vec2 pos; vec2 halfSize; float alpha = 0.0;
  v_kind = kind;
  if (kind < 0.0) {
    // Comet: head travels from p0 to p1 over dur; a 40% tail follows.
    vec2 p1 = (u_matrix * vec3(a_ends.zw, 1.0)).xy;
    float head = clamp(t / dur, 0.0, 1.0);
    float tail = clamp(t / dur - 0.4, 0.0, 1.0);
    vec2 a = mix(p0, p1, tail), b = mix(p0, p1, head);
    vec2 d = b - a; float len = max(length(d), 1.0);
    vec2 dir = d / len, nrm = vec2(-dir.y, dir.x);
    float w = max(1.2, a_shape.x);
    pos = (a + b) * 0.5 + dir * a_corner.x * (len * 0.5 + w) + nrm * a_corner.y * w;
    v_uv = vec2(a_corner.x, a_corner.y);
    alpha = (t >= 0.0 && t <= dur * 1.4) ? a_shape.y * (1.0 - clamp((t - dur) / (dur * 0.4), 0.0, 1.0)) : 0.0;
  } else {
    float r = a_shape.x * u_scale;
    float life = t < attack ? t / attack : 1.0 - (t - attack) / dur;
    alpha = (t >= 0.0) ? clamp(life, 0.0, 1.0) * a_shape.y : 0.0;
    if (kind > 0.5 && kind < 1.5) r *= 1.0 + 0.8 * clamp(t / (attack + dur), 0.0, 1.0);
    pos = p0 + a_corner * r;
    v_uv = a_corner;
  }
  v_alpha = alpha;
  v_color = a_color.rgb;
  vec2 clip = pos / u_size * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
}`;

const FRAG = `#version 300 es
precision mediump float;
in vec2 v_uv;
in float v_alpha;
in vec3 v_color;
flat in float v_kind;
out vec4 outColor;
void main() {
  float d = length(v_uv);
  float a;
  if (v_kind < 0.0) {
    // Bright head at +1, fading tail toward -1; soft across the width.
    float along = (v_uv.x + 1.0) * 0.5;
    a = v_alpha * (0.15 + 0.85 * along * along) * (1.0 - abs(v_uv.y));
  } else if (v_kind > 0.5 && v_kind < 1.5) {
    a = v_alpha * smoothstep(0.08, 0.0, abs(d - 0.85));
  } else {
    // Radial glow: 0.3 at the center → 0.12 → 0 at the edge.
    a = v_alpha * mix(0.3, 0.0, smoothstep(0.0, 1.0, d)) / 0.3 * (d < 1.0 ? 1.0 : 0.0);
  }
  // Premultiplied; overlapping light adds up but each fragment is capped.
  a = min(a, 0.9);
  outColor = vec4(v_color * a, a);
}`;

function compile(gl: WebGL2RenderingContext, type: number, src: string) {
  const s = gl.createShader(type)!;
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) ?? 'shader');
  return s;
}

export class LightLayer {
  private gl: WebGL2RenderingContext;
  private program: WebGLProgram;
  private buffer: WebGLBuffer;
  private data = new Float32Array(MAX_INSTANCES * FLOATS);
  private count = 0;
  private next = 0;
  private endsAt = 0;
  private raf = 0;
  private loc: Record<string, WebGLUniformLocation | null> = {};
  /** Frame timestamps while animating, for the perf harness. */
  frames: number[] = [];

  constructor(
    private canvas: HTMLCanvasElement,
    private toPixels: () => { m: number[]; scale: number },
    private onFrame?: () => void,
  ) {
    const gl = canvas.getContext('webgl2', { premultipliedAlpha: true, alpha: true, antialias: true });
    if (!gl) throw new Error('WebGL2 unavailable');
    this.gl = gl;
    const p = gl.createProgram()!;
    gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, VERT));
    gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p) ?? 'link');
    this.program = p;
    for (const u of ['u_matrix', 'u_size', 'u_now', 'u_scale']) this.loc[u] = gl.getUniformLocation(p, u);

    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    const corners = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, corners);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const cl = gl.getAttribLocation(p, 'a_corner');
    gl.enableVertexAttribArray(cl);
    gl.vertexAttribPointer(cl, 2, gl.FLOAT, false, 0, 0);

    this.buffer = gl.createBuffer()!;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.buffer);
    gl.bufferData(gl.ARRAY_BUFFER, this.data.byteLength, gl.DYNAMIC_DRAW);
    const stride = FLOATS * 4;
    const attr = (name: string, size: number, offset: number) => {
      const l = gl.getAttribLocation(p, name);
      gl.enableVertexAttribArray(l);
      gl.vertexAttribPointer(l, size, gl.FLOAT, false, stride, offset * 4);
      gl.vertexAttribDivisor(l, 1);
    };
    attr('a_ends', 4, 0);
    attr('a_time', 4, 4);
    attr('a_shape', 2, 8);
    attr('a_color', 4, 10);
  }

  private push(values: number[], end: number) {
    const i = this.next;
    this.data.set(values, i * FLOATS);
    // One upload per instance, at the time of the event.
    this.gl.bindBuffer(this.gl.ARRAY_BUFFER, this.buffer);
    this.gl.bufferSubData(this.gl.ARRAY_BUFFER, i * FLOATS * 4, this.data, i * FLOATS, FLOATS);
    this.next = (this.next + 1) % MAX_INSTANCES; // oldest light is dropped past the cap
    this.count = Math.min(MAX_INSTANCES, this.count + 1);
    this.endsAt = Math.max(this.endsAt, end);
  }

  pulse(p: Pulse) {
    this.push([p.x0, p.y0, p.x1, p.y1, p.start, p.duration, 0, -1, p.width, p.brightness, ...p.color, 0], p.start + p.duration * 1.4);
  }

  glow(g: Glow) {
    this.push([g.x, g.y, 0, 0, g.start, g.decay, g.attack, g.kind, g.radius, g.intensity, ...g.color, 0], g.start + g.attack + g.decay);
  }

  /** Start drawing; stops by itself once the last light has faded. */
  wake() {
    if (!this.raf) this.raf = requestAnimationFrame(this.frame);
  }

  get animating() {
    return this.raf !== 0;
  }

  private frame = (now: number) => {
    this.raf = 0;
    const t = performance.now();
    this.frames.push(t);
    if (this.frames.length > 600) this.frames.splice(0, 300);
    this.draw(t);
    this.onFrame?.();
    if (t < this.endsAt) this.raf = requestAnimationFrame(this.frame);
    else this.clear();
    void now;
  };

  draw(now = performance.now()) {
    const gl = this.gl;
    const dpr = window.devicePixelRatio || 1;
    const w = this.canvas.clientWidth, h = this.canvas.clientHeight;
    if (this.canvas.width !== w * dpr || this.canvas.height !== h * dpr) {
      this.canvas.width = w * dpr;
      this.canvas.height = h * dpr;
    }
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(this.program);
    const { m, scale } = this.toPixels();
    gl.uniformMatrix3fv(this.loc.u_matrix, false, m);
    gl.uniform2f(this.loc.u_size, w, h);
    gl.uniform1f(this.loc.u_now, now);
    gl.uniform1f(this.loc.u_scale, scale);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, this.count);
  }

  clear() {
    const gl = this.gl;
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);
  }

  dispose() {
    cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.gl.getExtension('WEBGL_lose_context')?.loseContext();
  }
}

/** Affine graph→pixel matrix (column-major mat3) from three projected points. */
export function affine(project: (p: { x: number; y: number }) => { x: number; y: number }): number[] {
  const o = project({ x: 0, y: 0 }), ex = project({ x: 1, y: 0 }), ey = project({ x: 0, y: 1 });
  return [ex.x - o.x, ex.y - o.y, 0, ey.x - o.x, ey.y - o.y, 0, o.x, o.y, 1];
}

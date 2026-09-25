// VERBATIM from the LeCodes SDK (`sdk/src/core/color.ts`), the import pointed at the vendored twin beside it.
// The one color conversion of the SDK. Users pass colors the natural ways — any CSS color string
// ('#10131a', '#abc', 'red', 'rgb(0 0 0 / 50%)', 'hsl(210 50% 40%)', 'transparent'), a packed number
// (0xff0000 = opaque 0xRRGGBB) or normalized components ([r,g,b] / [r,g,b,a] in 0..1). The engines
// want different forms (2D wants 0..1 floats, 3D a packed int, the UI core '#rrggbbaa'), so this
// module is the single place that converts. Strings go through the vendored AnyCanvas twin of
// anycanvas/css_color.h (the parser every native layer uses), so the SDK and the engines agree on
// every name and every byte. Anything that is not a color throws with the value.

import { cssToByte, parseCssColor } from "./cssColor"

export type ColorInput =
  | string
  | number
  | readonly [number, number, number]
  | readonly [number, number, number, number]

type Rgba = [number, number, number, number]

const f = Math.fround

// The input as four floats, or null when it is not a color. Arrays pass through losslessly (HDR
// components above 1 stay); a number is an integer 0..0xFFFFFF.
const parse = (c: unknown): Rgba | null => {
  if (typeof c === "string") {
    const p = parseCssColor(c)
    return p ? [ p.r, p.g, p.b, p.a ] : null
  }
  if (typeof c === "number") {
    if (!Number.isInteger(c) || c < 0 || c > 0xFFFFFF) return null
    return [ f(((c >> 16) & 255) / 255), f(((c >> 8) & 255) / 255), f((c & 255) / 255), 1 ]
  }
  if (typeof c === "object" && c !== null && typeof (c as ArrayLike<number>).length === "number") {
    const a = c as ArrayLike<unknown>
    if (a.length !== 3 && a.length !== 4) return null
    for (let i = 0; i < a.length; i++) if (typeof a[i] !== "number" || !Number.isFinite(a[i])) return null
    const n = a as ArrayLike<number>
    return [ n[0], n[1], n[2], a.length === 4 ? n[3] : 1 ]
  }
  return null
}

const show = (c: unknown): string =>
  typeof c === "string" ? JSON.stringify(c)
    : typeof c === "number" && Number.isInteger(c) && c >= 0 ? `0x${c.toString(16)} (${c})`
      : Array.isArray(c) ? `[${c.join(", ")}]` : String(c)

/** Normalize any ColorInput to four 0..1 float components; throws with the value when it is not one. */
const toFloats = (c: ColorInput): Rgba => {
  const rgba = parse(c)
  if (rgba) return rgba
  if (typeof c === "number") {
    throw new RangeError(`Color: ${show(c)} is not a color — a number is an opaque 0xRRGGBB (an integer 0..0xFFFFFF); write alpha as "#rrggbbaa" or [r, g, b, a]`)
  }
  if (typeof c === "string") throw new TypeError(`Color: ${show(c)} is not a CSS color`)
  throw new TypeError(`Color: ${show(c)} is not a color — a string, an 0xRRGGBB number or [r, g, b(, a)] in 0..1`)
}

// Bytes by the one rounding rule of the engines (anycanvas css::toByte: float32, clamped, half up).
const toBytes = (c: ColorInput): Rgba => {
  const v = toFloats(c)
  return [ cssToByte(v[0]), cssToByte(v[1]), cssToByte(v[2]), cssToByte(v[3]) ]
}

const hex2 = (n: number): string => (n < 16 ? "0" : "") + n.toString(16)

export const Color = {
  /** [r,g,b] in 0..1 — for the 2D engine's float color setters. */
  toRgb01(c: ColorInput): [number, number, number] {
    const v = toFloats(c)
    return [ v[0], v[1], v[2] ]
  },
  /** [r,g,b,a] in 0..1. */
  toRgba01(c: ColorInput): [number, number, number, number] {
    return toFloats(c)
  },
  /** 0xRRGGBB packed int — for the 3D engine's color uniforms / skybox. */
  toPackedRgb(c: ColorInput): number {
    const b = toBytes(c)
    return (b[0] << 16) | (b[1] << 8) | b[2]
  },
  /** 0xRRGGBBAA packed int (unsigned). */
  toPackedRgba(c: ColorInput): number {
    const b = toBytes(c)
    return ((b[0] << 24) | (b[1] << 16) | (b[2] << 8) | b[3]) >>> 0
  },
  /** '#rrggbbaa' (lower-case, always eight digits) — the UI wire form and the 3D material color. */
  toHexString(c: ColorInput): string {
    const b = toBytes(c)
    return "#" + hex2(b[0]) + hex2(b[1]) + hex2(b[2]) + hex2(b[3])
  },
  /** [r,g,b,a] in 0..1, or `null` when `c` is not a color — the non-throwing probe. */
  tryParse(c: unknown): [number, number, number, number] | null {
    return parse(c)
  },
}

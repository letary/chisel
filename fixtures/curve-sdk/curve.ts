// The particle curve builders, VERBATIM from the LeCodes SDK (`sdk/src/gl/Particles.ts`) — minus the
// parts that talk to the native bridge — over the SDK's own `Color` (`color.ts` beside this file, with
// its vendored AnyCanvas twin `cssColor.ts`). This is the contract `chisel-core/src/curve.rs` mirrors
// op-for-op: the builders' method bodies here, the colors through the `anycanvas-css-color` crate.
//
// Used by `crates/chisel-core/tests/curve.rs` and `scripts/curve-exact.mjs` (which bundles the same
// sources with fusion on and off and compares the resulting `_data` buffers element for element).

import { Color, type ColorInput } from "./color"

type Range<T> = T | { min: T, max: T }

const isColorRange = (v: unknown): v is { min: ColorInput, max: ColorInput } =>
  typeof v === "object" && v !== null && !Array.isArray(v) && "min" in v

const lohi = (v: Range<number>): [number, number] =>
  typeof v === "number" ? [ v, v ] : [ v.min, v.max ]

export class CurveBuilder {
  private _kind: 0 | 1
  private _lo: number
  private _hi: number
  private _stops: number[] = []   // flat (t, lo, hi)

  constructor(base: Range<number>, mode: "multiply" | "add") {
    this._kind = mode === "add" ? 1 : 0
    ;[ this._lo, this._hi ] = lohi(base)
  }

  from(v: Range<number>): this {
    if (this._stops.length > 0) throw new Error("curve: .from() must come first")
    return this.via(0, v)
  }

  via(t: number, v: Range<number>): this {
    const [ lo, hi ] = lohi(v)
    this._stops.push(t, lo, hi)
    return this
  }

  to(v: Range<number>): this {
    return this.via(1, v)
  }

  fade(fadeIn = 0.15, fadeOut = fadeIn): this {
    const up = Math.min(Math.max(fadeIn, 0), 1)
    const down = Math.min(Math.max(fadeOut, 0), 1 - up)
    if (up > 0) this.via(0, 0).via(up, 1)
    else this.via(0, 1)
    if (down > 0) {
      if (1 - down > up) this.via(1 - down, 1)
      this.via(1, 0)
    }
    return this
  }

  get _data(): Float32Array {
    const stops = this._stops
    const prepend = stops.length > 0 && stops[0] > 0
    const n = stops.length / 3 + (prepend ? 1 : 0)
    const out = new Float32Array(4 + n * 3)
    out[0] = this._kind
    out[1] = this._lo
    out[2] = this._hi
    out[3] = n
    let o = 4
    if (prepend) {
      const identity = this._kind === 1 ? 0 : 1
      out[o + 1] = identity
      out[o + 2] = identity
      o += 3
    }
    out.set(stops, o)
    return out
  }
}

export class ColorCurveBuilder {
  private _lo: [number, number, number, number]
  private _hi: [number, number, number, number]
  private _stops: number[] = []   // flat (t, lo rgba, hi rgba)

  constructor(base: Range<ColorInput>) {
    if (isColorRange(base)) {
      this._lo = Color.toRgba01(base.min)
      this._hi = Color.toRgba01(base.max)
    } else {
      this._lo = Color.toRgba01(base)
      this._hi = this._lo
    }
  }

  from(c: Range<ColorInput>): this {
    if (this._stops.length > 0) throw new Error("colorCurve: .from() must come first")
    return this.via(0, c)
  }

  via(t: number, c: Range<ColorInput>): this {
    let lo: number[], hi: number[]
    if (isColorRange(c)) {
      lo = Color.toRgba01(c.min)
      hi = Color.toRgba01(c.max)
    } else {
      lo = hi = Color.toRgba01(c)
    }
    this._stops.push(t, ...lo, ...hi)
    return this
  }

  to(c: Range<ColorInput>): this {
    return this.via(1, c)
  }

  get _data(): Float32Array {
    const stops = this._stops
    const prepend = stops.length > 0 && stops[0] > 0
    const n = stops.length / 9 + (prepend ? 1 : 0)
    const out = new Float32Array(9 + n * 9)
    out.set(this._lo, 0)
    out.set(this._hi, 4)
    out[8] = n
    let o = 9
    if (prepend) {
      for (let i = 1; i < 9; i++) out[o + i] = 1
      o += 9
    }
    out.set(stops, o)
    return out
  }
}

export const curve = (base: Range<number> = 1, mode: "multiply" | "add" = "multiply"): CurveBuilder =>
  new CurveBuilder(base, mode)

export const colorCurve = (base: Range<ColorInput> = "#ffffff"): ColorCurveBuilder =>
  new ColorCurveBuilder(base)

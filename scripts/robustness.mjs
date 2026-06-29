#!/usr/bin/env node
// Robustness sweep: bundle a broad corpus of JS/TS syntax through chisel (standalone, no SDK) and
// check each survives. Every snippet is self-verifying — it throws on a wrong result, so "runs"
// means "correct". Each is bundled twice: readable and minified (to catch mangler bugs). A case that
// crashes chisel, produces invalid JS, or throws at runtime is a contained gap to fix.
//   node scripts/robustness.mjs [--show <name>]

import { spawnSync } from "node:child_process"
import { existsSync } from "node:fs"
import { join } from "node:path"
import vm from "node:vm"

const bin = ["target/release/chisel", "target/debug/chisel"].map((p) => join(import.meta.dirname, "..", p)).find(existsSync)
if (!bin) { console.error("build chisel first"); process.exit(2) }
const showOnly = process.argv.includes("--show") ? process.argv[process.argv.indexOf("--show") + 1] : null

// Each case: code asserts its own correctness via `assert`. `fuse` opts a case into math fusion.
const A = "function assert(c,m){ if(!c) throw new Error('assert: '+(m||'')) }\n"
// A Vec3-shaped class shared by the fusion stress cases.
const V3 = "class Vec3{constructor(x,y,z){this.x=x;this.y=y;this.z=z}" +
  " add(v){return new Vec3(this.x+v.x,this.y+v.y,this.z+v.z)}" +
  " scale(s){return new Vec3(this.x*s,this.y*s,this.z*s)}" +
  " dot(v){return this.x*v.x+this.y*v.y+this.z*v.z}" +
  " length(){return Math.hypot(this.x,this.y,this.z)}" +
  " cross(v){return new Vec3(this.y*v.z-this.z*v.y,this.z*v.x-this.x*v.z,this.x*v.y-this.y*v.x)}" +
  " normalize(){const l=Math.hypot(this.x,this.y,this.z); return l===0?new Vec3(0,0,0):new Vec3(this.x/l,this.y/l,this.z/l)}}\n"
const cases = [
  // --- declarations / destructuring ---
  ["const-let-var", `let a=1; var b=2; const c=3; assert(a+b+c===6)`],
  ["array-destructure", `const [x,,y,...rest]=[1,2,3,4,5]; assert(x===1&&y===3&&rest.length===2)`],
  ["object-destructure", `const {a,b:{c}={c:9},d=10}={a:1,b:{c:2}}; assert(a===1&&c===2&&d===10)`],
  ["swap-destructure", `let a=1,b=2; [a,b]=[b,a]; assert(a===2&&b===1)`],
  ["default-params", `const f=(a,b=a*2,...r)=>a+b+r.length; assert(f(3)===9 && f(1,1,9,9)===4)`],
  // --- operators ---
  ["optional-chain", `const o={a:{b:()=>5}}; assert((o?.a?.b?.()??0)===5 && (o?.x?.y??7)===7)`],
  ["nullish-logical-assign", `let a=null; a??=4; let b=0; b||=5; let c=1; c&&=6; assert(a===4&&b===5&&c===6)`],
  ["exp-and-comma", `const x=2**10; const y=(1,2,3); assert(x===1024&&y===3)`],
  ["bigint-numsep", `const n=1_000_000; const big=9007199254740993n; assert(n===1000000 && big>9007199254740992n)`],
  // --- classes ---
  ["class-private", `class C{#x=1; #m(){return this.#x+1} get v(){return this.#m()} static #s=9; static get s(){return C.#s}} const c=new C(); assert(c.v===2&&C.s===9)`],
  ["class-static-block", `class C{static x; static{ C.x=42 }} assert(C.x===42)`],
  ["class-inherit-super", `class A{constructor(n){this.n=n} g(){return this.n}} class B extends A{constructor(){super(7)} g(){return super.g()*2}} assert(new B().g()===14)`],
  ["class-computed-getter", `const k='dyn'; class C{['m'+1](){return 3} get [k](){return 4}} const c=new C(); assert(c.m1()===3&&c.dyn===4)`],
  ["class-expr", `const C=class{v(){return 5}}; assert(new C().v()===5)`],
  // --- control flow ---
  ["for-of-in", `let s=0; for(const x of [1,2,3]) s+=x; let k=''; for(const p in {a:1,b:2}) k+=p; assert(s===6&&k==='ab')`],
  ["try-finally", `let log=''; function f(){try{return 1}finally{log+='f'}} const r=f(); assert(r===1&&log==='f')`],
  ["switch-fallthrough", `function f(n){switch(n){case 1: case 2: return 'lo'; default: return 'hi'}} assert(f(2)==='lo'&&f(9)==='hi')`],
  ["labeled-break", `let c=0; outer: for(let i=0;i<3;i++){for(let j=0;j<3;j++){if(j===1)continue outer; c++}} assert(c===3)`],
  // --- functions ---
  ["generator", `function* g(){yield 1; yield* [2,3]} assert([...g()].join('')==='123')`],
  ["closures-iife", `const c=(()=>{let n=0; return ()=>++n})(); assert(c()===1&&c()===2)`],
  ["tagged-template", `function tag(s,...v){return s.join('|')+v.join(',')} assert(tag\`a\${1}b\${2}\`==='a|b|1,2')`],
  ["spread-call", `function sum(...a){return a.reduce((x,y)=>x+y,0)} const arr=[1,2,3]; assert(sum(...arr,4)===10)`],
  ["object-spread", `const a={x:1}; const b={...a,y:2}; assert(b.x===1&&b.y===2)`],
  // --- TS-specific (must survive strip) ---
  ["ts-enum", `enum E{A,B,C} assert(E.A===0&&E.C===2&&E[1]==='B')`],
  ["ts-const-enum", `const enum E{X=10,Y} const v=E.Y; assert(v===11)`],
  ["ts-namespace", `namespace N{ export const x=5; export function f(){return x*2} } assert(N.f()===10)`],
  ["ts-param-props", `class P{constructor(public a:number, private b:number){} sum(){return this.a+this.b}} assert(new P(2,3).sum()===5)`],
  ["ts-generics-asconst", `function id<T>(x:T):T{return x} const t=[1,2] as const; assert(id(t)[1]===2)`],
  ["ts-abstract", `abstract class Sh{abstract area():number; describe(){return this.area()}} class Sq extends Sh{constructor(private s:number){super()} area(){return this.s*this.s}} assert(new Sq(3).describe()===9)`],
  ["ts-nonnull-satisfies", `const o={a:1} satisfies Record<string,number>; const v:number|undefined=o.a; assert(v!+1===2)`],
  // --- modules ---
  ["default-export-chain", `assert(true)`, { extra: { "/lib.ts": `export default function(){return 42}` }, head: `import f from './lib'\n` }],
  ["named-and-rexport", `assert(leaf()==='leaf' && hub()==='hub')`, { extra: { "/hub.ts": `export { leaf } from './leaf'\nexport function hub(){ return 'hub' }`, "/leaf.ts": `export function leaf(){return 'leaf'}` }, head: `import { leaf, hub } from './hub'\n` }],
  ["circular", `assert(ping(3)===0)`, { extra: { "/a.ts": `import { pong } from './b'\nexport function ping(n){ return n<=0?0:pong(n-1) }`, "/b.ts": `import { ping } from './a'\nexport function pong(n){ return ping(n-1) }` }, head: `import { ping } from './a'\n` }],
  // --- instance-method DCE stress: a method reachable only via a non-obvious access form must
  //     survive presence-gating (else it's wrongly dropped) ---
  ["dce-optional-call", `class C{ping(){return 7} dead(){return 0}} const c=new C(); assert((c?.ping?.()??-1)===7)`],
  ["dce-computed-string", `class C{ping(){return 7}} const c=new C(); assert(c["ping"]()===7)`],
  ["dce-method-callback", `class C{val(){return 9}} const c=new C(); const f=c.val.bind(c); const g=()=>c.val(); assert(f()===9&&g()===9)`],
  ["dce-method-ref-arg", `class C{tick(){return 3}} function call(fn){return fn()} const c=new C(); assert(call(()=>c.tick())===3)`],
  ["dce-getter-only", `class C{#n=5; get n(){return this.#n} set n(v){this.#n=v}} const c=new C(); c.n=8; assert(c.n===8)`],
  // --- optimizer stress: fusion on a local Vec3-shaped class (self-verifying numbers) ---
  ["fusion-chain", `${V3}const a=new Vec3(1,2,3),b=new Vec3(4,5,6); const r=a.add(b).scale(2).dot(b); assert(r===2*((1+4)*4+(2+5)*5+(3+6)*6))`, { fuse: true }],
  ["fusion-nested-arg", `${V3}const a=new Vec3(1,2,3),b=new Vec3(4,5,6),c=new Vec3(1,1,1); const r=a.add(b.scale(2)).dot(c); assert(r===9+12+15)`, { fuse: true }],
  ["fusion-ternary", `${V3}const a=new Vec3(1,2,3),b=new Vec3(4,5,6),c=new Vec3(1,1,1); const r=(1<2)?a.add(b).dot(c):0; assert(r===21)`, { fuse: true }],
  ["fusion-scalar-arith", `${V3}const a=new Vec3(1,2,3),b=new Vec3(4,5,6); const r=a.length()+b.length(); assert(r===Math.hypot(1,2,3)+Math.hypot(4,5,6))`, { fuse: true }],
  ["fusion-reused-result", `${V3}const a=new Vec3(1,2,3),b=new Vec3(4,5,6),c=new Vec3(1,1,1); const d=a.add(b); const r=d.dot(c)+d.x; assert(r===21+5)`, { fuse: true }],
  ["fusion-in-array", `${V3}const a=new Vec3(1,2,3),b=new Vec3(4,5,6),c=new Vec3(1,1,1); const arr=[a.add(b).x, a.dot(c)]; assert(arr[0]===5&&arr[1]===6)`, { fuse: true }],
  ["fusion-cross-normalize", `${V3}const a=new Vec3(1,0,0),b=new Vec3(0,1,0); const n=a.cross(b).normalize(); assert(n.x===0&&n.y===0&&n.z===1)`, { fuse: true }],
]

const runChisel = (files, entry, minify, fuse) => {
  const r = spawnSync(bin, [], { input: JSON.stringify({ files, entry, inject: [], format: "esm", minify, fuse: !!fuse }), encoding: "utf8", maxBuffer: 64 << 20 })
  if (r.status === 2) return { fatal: (r.stderr || "crash").split("\n")[0] }
  try { return JSON.parse(r.stdout || "{}") } catch { return { fatal: "bad json: " + (r.stdout || r.stderr || "").slice(0, 80) } }
}

const builtins = { Math, Object, Array, JSON, Number, String, Boolean, Symbol, Map, Set, WeakMap, WeakSet, Promise, Proxy, Reflect, BigInt, RegExp, Error, TypeError, RangeError, Date, parseInt, parseFloat, isNaN, isFinite, Float32Array, Uint8Array, Int32Array, ArrayBuffer, DataView, console: { log() {}, warn() {}, error() {} }, structuredClone: (x) => x }
const runCode = (code) => {
  try { vm.runInContext(code, vm.createContext({ ...builtins }), { timeout: 4000 }); return "ok" }
  catch (e) { return `${e.constructor.name}: ${String(e.message).slice(0, 90)}` }
}

let pass = 0, fail = 0
for (const [name, body, opts = {}] of cases) {
  if (showOnly && name !== showOnly) continue
  const files = { "/main.ts": A + (opts.head || "") + body, ...(opts.extra || {}) }
  // assert must be in scope of extra modules too — prepend to each.
  if (opts.extra) for (const k of Object.keys(opts.extra)) files[k] = A + files[k]
  let verdict = "ok"
  for (const minify of [false, true]) {
    const out = runChisel(files, "/main.ts", minify, opts.fuse)
    if (out.fatal) { verdict = `chisel ${minify ? "(min) " : ""}FATAL: ${out.fatal}`; break }
    if (out.error) { verdict = `chisel ${minify ? "(min) " : ""}error: ${out.error}`; break }
    if (showOnly) { console.log(`\n--- ${name} ${minify ? "(min)" : "(readable)"} ---\n${out.code}`); continue }
    const ran = runCode(out.code)
    if (ran !== "ok") { verdict = `${minify ? "(min) " : ""}runtime: ${ran}`; break }
  }
  if (showOnly) continue
  const ok = verdict === "ok"
  ok ? pass++ : fail++
  console.log(`  ${ok ? "✓" : "✗"} ${name.padEnd(24)} ${ok ? "" : verdict}`)
}
if (!showOnly) console.log(`\n${pass}/${pass + fail} passed${fail ? `, ${fail} FAILED` : ""}`)
process.exit(fail ? 1 : 0)

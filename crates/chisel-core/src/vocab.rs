//! The SDK's vocabulary: every SDK name a pass keys on.
//!
//! chisel's passes are algorithms over SDK-shaped code (UI factory calls and builder chains, signal
//! reads, Vec3 / date math, particle curves, component writes, the asset macro). The NAMES they key
//! on belong to the SDK and change with it, so the SDK declares them in one manifest (LeCodes:
//! `sdk/src/chisel.ts`) and the bundler passes it as `Input.vocab`. A bundle with `vocab` uses the
//! manifest's entries in place of the built-in ones — a list the manifest gives replaces the default
//! list outright, nothing is merged. Without `vocab` (an SDK older than the manifest) the built-in
//! defaults below apply: they are the schema-1 manifest, kept for one release as that fallback.
//!
//! Schema rule: `schema` is the manifest's contract version. A NEWER schema than [`SCHEMA`] is a hard
//! error (the SDK needs a newer chisel). A group, field or role the manifest omits takes its default
//! — that is how a manifest of an older schema, written before a group existed, stays readable (so
//! the defaults of a group stay as long as an older schema may lack it). A role is switched off by
//! mapping it to `""`, never by leaving it out. An unknown field is an error. A chisel release is
//! then needed only when a pass's algorithm changes or the schema grows; a rename in the SDK is a
//! manifest edit.
//!
//! What is NOT vocabulary, and stays in the passes: JS builtins and language (`Math.hypot`,
//! `Date.now`, `Float32Array`, `Object.assign`, `Array.prototype.map`, `undefined`, `typeof`
//! strings, the `default` export), chisel's own synthesized names (`__chisel_*`), and the
//! semantics of a lowering — the calling conventions of the helpers it emits (argument order, the
//! `__compOp` op codes), the string values a mirrored method body interprets (curve modes, date
//! units), default arguments and native limits. Changing any of those changes what a pass computes,
//! which is an algorithm change.

use std::collections::HashMap;

use serde::Deserialize;

/// The manifest schema this chisel implements.
pub const SCHEMA: u64 = 1;

/// The whole manifest. Every group is optional on the wire: a missing one takes its default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Vocab {
    /// The manifest's contract version (checked by [`Vocab::from_input`] before anything else).
    pub schema: u64,
    pub ui: Ui,
    pub signals: Signals,
    pub fusion: Fusion,
    pub comp_write: CompWrite,
    pub assets: Assets,
}

impl Vocab {
    /// The vocabulary of one bundle: `Input.vocab` when present, else the built-in defaults. The
    /// schema is read first, so a newer manifest fails with an upgrade message rather than whatever
    /// shape error its new fields would produce.
    pub fn from_input(raw: Option<&serde_json::Value>) -> anyhow::Result<Vocab> {
        let Some(raw) = raw else { return Ok(Vocab::default()) };
        let Some(given) = raw.get("schema") else {
            anyhow::bail!("vocab: missing `schema` (the manifest's integer contract version)");
        };
        let schema = given
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("vocab: `schema` must be a non-negative integer, got {given}"))?;
        if schema > SCHEMA {
            anyhow::bail!(
                "vocab: the SDK's vocabulary manifest is schema {schema}, this chisel implements schema {SCHEMA} — upgrade @letary/chisel"
            );
        }
        serde_json::from_value(raw.clone()).map_err(|e| anyhow::anyhow!("vocab: {e}"))
    }
}

impl Default for Vocab {
    fn default() -> Self {
        Vocab {
            schema: SCHEMA,
            ui: Ui::default(),
            signals: Signals::default(),
            fusion: Fusion::default(),
            comp_write: CompWrite::default(),
            assets: Assets::default(),
        }
    }
}

/// `flatten_ui` + `reactive_ui`: the UI factories and the builder API.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Ui {
    /// Every factory whose runtime goes through the SDK's variadic `buildUI` dispatch (plus
    /// `UIPager`, which flattens tab arguments the same way) — the array-splice targets.
    pub factories: Vec<String>,
    /// Factory → its dispatch-free builder (`UIColumn` → `__UIColumn`, an inject export) — the
    /// raw-lowering targets.
    pub raw: HashMap<String, String>,
    /// The other free calls that return a node (argument provability: `UIText`, `UIImage`, …).
    /// A node-returning call is one of `factories`, `leaves`, or a `raw` builder.
    pub leaves: Vec<String>,
    /// Container factories whose function-valued children argument is a reactive children binding.
    pub containers: Vec<String>,
    /// The text factory: its (last) text argument is a binding position.
    pub text: String,
    /// Builder methods that return the node (`this`): a chain rooted at a node factory stays a
    /// node. A conservative whitelist — an unlisted method blocks provability.
    pub chain: Vec<String>,
    /// The children setter: `x.setContent(() => …)`'s closure is a children-position closure.
    pub set_content: String,
    /// Member calls whose object-literal argument holds bindings (`.style({…})`, `.class({…})`):
    /// their top-level values are auto-wrapped like a factory's style argument.
    pub binding_methods: Vec<String>,
    /// The memoized children-map helper (`__uiMap(list, fn, slot)`, an inject export).
    pub map_helper: String,
}

impl Ui {
    /// A free call to `name` provably returns a node.
    pub fn returns_node(&self, name: &str) -> bool {
        has(&self.factories, name) || has(&self.leaves, name) || self.raw.values().any(|r| r == name)
    }
}

impl Default for Ui {
    fn default() -> Self {
        const CONTAINERS: &[&str] = &["UIColumn", "UIRow", "UIBox", "UIButton", "UIScrollable", "UIScreen", "UIWidget"];
        Ui {
            factories: strings(&[
                "UIColumn", "UIRow", "UIBox", "UIButton", "UIScrollable", "UIScreen", "UIWidget",
                "UIModal", "UIPopover", "UIBottomSheet", "UIPager",
            ]),
            raw: CONTAINERS.iter().map(|f| (f.to_string(), format!("__{f}"))).collect(),
            leaves: strings(&["UIText", "UIImage", "UIVideo", "UIInput", "UITextArea", "UISpacer"]),
            containers: strings(CONTAINERS),
            text: "UIText".into(),
            chain: strings(&[
                "style", "animateTo", "animateFrom", "class", "named",
                "onClick", "onTouchStart", "onLongPress", "onMouseEnter", "onLayout", "onOpen", "onClose", "onBack",
                "onScroll", "onScrollRelease", "onOverscroll", "onRefresh", "onSubmit",
                "onSelect", "onChange", "onFocus", "onBlur", "onOverlayTap", "onDetent",
                "onEndReached", "onStartReached",
                "append", "insert", "remove", "setContent", "keepAlive",
            ]),
            set_content: "setContent".into(),
            binding_methods: strings(&["style", "class"]),
            map_helper: "__uiMap".into(),
        }
    }
}

/// `reactive_ui`'s signal-type inference.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Signals {
    /// Free calls that root the inference (`signal(…)`, `computed(…)`).
    pub factories: Vec<String>,
    /// The property a signal read goes through (`s.value`).
    pub value: String,
}

impl Default for Signals {
    fn default() -> Self {
        Signals { factories: strings(&["signal", "computed"]), value: "value".into() }
    }
}

/// The chain fusers (`fusion.rs`, `curve.rs`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Fusion {
    pub vec3: Vec3,
    pub date: Date,
    pub curve: Curve,
}

/// Vec3 chain fusion.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Vec3 {
    /// The class (an inject export): `new Vec3(…)` roots a chain and materializes an escaping one.
    pub class: String,
    /// The component fields, in order (a Vec3-typed local `v` decomposes to `v.x, v.y, v.z`).
    pub fields: [String; 3],
    /// Statics that yield a Vec3 (`Vec3.up`, `Vec3.from(…)`) — local type inference.
    pub statics: Vec<String>,
    /// Instance methods that return a Vec3 — local type inference (broader than the fused set).
    pub returning: Vec<String>,
    /// The methods the pass lowers, by role.
    pub methods: Vec3Methods,
}

impl Default for Vec3 {
    fn default() -> Self {
        Vec3 {
            class: "Vec3".into(),
            fields: ["x".into(), "y".into(), "z".into()],
            statics: strings(&["up", "down", "left", "right", "forward", "back", "zero", "one", "from"]),
            returning: strings(&[
                "add", "sub", "mul", "div", "scale", "negate", "scaleAndAdd", "cross", "lerp", "clamp", "min",
                "max", "reflect", "project", "rotateX", "rotateY", "rotateZ", "rotate", "transform", "withX",
                "withY", "withZ", "clone", "normalize", "copy", "set",
            ]),
            methods: Vec3Methods::default(),
        }
    }
}

/// A Vec3 method the fuser lowers. The lowering of each role mirrors that method's body in the SDK
/// (the algorithm); the manifest only names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vec3Op {
    Add,
    Sub,
    Mul,
    Scale,
    Negate,
    ScaleAndAdd,
    Cross,
    Normalize,
    Dot,
    LengthSq,
    Length,
    DistanceTo,
}

/// The SDK name of each [`Vec3Op`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Vec3Methods {
    pub add: String,
    pub sub: String,
    pub mul: String,
    pub scale: String,
    pub negate: String,
    pub scale_and_add: String,
    pub cross: String,
    pub normalize: String,
    pub dot: String,
    pub length_sq: String,
    pub length: String,
    pub distance_to: String,
}

impl Vec3Methods {
    /// The role `name` plays, if the fuser lowers it.
    pub fn op(&self, name: &str) -> Option<Vec3Op> {
        use Vec3Op::*;
        [
            (&self.add, Add),
            (&self.sub, Sub),
            (&self.mul, Mul),
            (&self.scale, Scale),
            (&self.negate, Negate),
            (&self.scale_and_add, ScaleAndAdd),
            (&self.cross, Cross),
            (&self.normalize, Normalize),
            (&self.dot, Dot),
            (&self.length_sq, LengthSq),
            (&self.length, Length),
            (&self.distance_to, DistanceTo),
        ]
        .into_iter()
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, op)| op)
    }
}

impl Default for Vec3Methods {
    fn default() -> Self {
        Vec3Methods {
            add: "add".into(),
            sub: "sub".into(),
            mul: "mul".into(),
            scale: "scale".into(),
            negate: "negate".into(),
            scale_and_add: "scaleAndAdd".into(),
            cross: "cross".into(),
            normalize: "normalize".into(),
            dot: "dot".into(),
            length_sq: "lengthSq".into(),
            length: "length".into(),
            distance_to: "distanceTo".into(),
        }
    }
}

/// `date()` chain fusion (single scalar: epoch ms).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Date {
    /// The factory global (`date(value?)`, an inject export).
    pub factory: String,
    /// The value class: an escaping chain materializes as `new DateValue(ms)`.
    pub class: String,
    /// `toMs(value): number` — the lowering normalizes any date input through it.
    pub to_ms: String,
    /// `formatImpl(ms, pattern, locale?)` — what a fused `.format()` calls.
    pub format_impl: String,
    /// `timeAgoImpl(ms, locale, nowMs)` — what a fused `.timeAgo()` calls.
    pub time_ago_impl: String,
    /// A value's epoch-ms field (a date-typed local `d` decomposes to `d.t`).
    pub field: String,
    /// Methods that return a date — local type inference (broader than the fused set:
    /// `startOf` / `endOf` yield a date the pass can read through `field` but not lower).
    pub returning: Vec<String>,
    /// The methods the pass lowers, by role.
    pub methods: DateMethods,
}

impl Default for Date {
    fn default() -> Self {
        Date {
            factory: "date".into(),
            class: "DateValue".into(),
            to_ms: "toMs".into(),
            format_impl: "formatImpl".into(),
            time_ago_impl: "timeAgoImpl".into(),
            field: "t".into(),
            returning: strings(&["add", "subtract", "startOf", "endOf"]),
            methods: DateMethods::default(),
        }
    }
}

/// A date method the fuser lowers (see [`Vec3Op`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOp {
    Add,
    Subtract,
    Format,
    TimeAgo,
    ValueOf,
    Unix,
    Diff,
    IsBefore,
    IsAfter,
    IsSame,
}

/// The SDK name of each [`DateOp`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct DateMethods {
    pub add: String,
    pub subtract: String,
    pub format: String,
    pub time_ago: String,
    pub value_of: String,
    pub unix: String,
    pub diff: String,
    pub is_before: String,
    pub is_after: String,
    pub is_same: String,
}

impl DateMethods {
    /// The role `name` plays, if the fuser lowers it.
    pub fn op(&self, name: &str) -> Option<DateOp> {
        use DateOp::*;
        [
            (&self.add, Add),
            (&self.subtract, Subtract),
            (&self.format, Format),
            (&self.time_ago, TimeAgo),
            (&self.value_of, ValueOf),
            (&self.unix, Unix),
            (&self.diff, Diff),
            (&self.is_before, IsBefore),
            (&self.is_after, IsAfter),
            (&self.is_same, IsSame),
        ]
        .into_iter()
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, op)| op)
    }
}

impl Default for DateMethods {
    fn default() -> Self {
        DateMethods {
            add: "add".into(),
            subtract: "subtract".into(),
            format: "format".into(),
            time_ago: "timeAgo".into(),
            value_of: "valueOf".into(),
            unix: "unix".into(),
            diff: "diff".into(),
            is_before: "isBefore".into(),
            is_after: "isAfter".into(),
            is_same: "isSame".into(),
        }
    }
}

/// Particle curve chains (`curve.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Curve {
    /// The numeric curve factory (`curve(base?, mode?)`, an inject export).
    pub factory: String,
    /// The color curve factory (`colorCurve(base?)`, an inject export).
    pub color_factory: String,
    /// The builders' payload getter: a fused chain becomes `{ <data>: new Float32Array([…]) }`,
    /// the duck type the runtime builder presents.
    pub data: String,
    /// The bounds of a `Range` object literal (`{ min, max }`).
    pub range: CurveRange,
    /// The stop methods, by role.
    pub stops: CurveStops,
}

impl Default for Curve {
    fn default() -> Self {
        Curve {
            factory: "curve".into(),
            color_factory: "colorCurve".into(),
            data: "_data".into(),
            range: CurveRange::default(),
            stops: CurveStops::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CurveRange {
    pub min: String,
    pub max: String,
}

impl Default for CurveRange {
    fn default() -> Self {
        CurveRange { min: "min".into(), max: "max".into() }
    }
}

/// A curve builder method that adds a stop (each one mutates and returns `this`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOp {
    From,
    Via,
    To,
    Fade,
}

/// The SDK name of each [`StopOp`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CurveStops {
    pub from: String,
    pub via: String,
    pub to: String,
    pub fade: String,
}

impl CurveStops {
    /// The role `name` plays, if it is a stop method.
    pub fn op(&self, name: &str) -> Option<StopOp> {
        use StopOp::*;
        [(&self.from, From), (&self.via, Via), (&self.to, To), (&self.fade, Fade)]
            .into_iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, op)| op)
    }
}

impl Default for CurveStops {
    fn default() -> Self {
        CurveStops { from: "from".into(), via: "via".into(), to: "to".into(), fade: "fade".into() }
    }
}

/// `comp_write`: component writes on SDK-owned vectors.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CompWrite {
    /// `__compWrite(owner, prop, axis, v)` — an inject export.
    pub write: String,
    /// `__compOp(owner, prop, axis, opCode, v)` — an inject export.
    pub op: String,
    /// The owner's instance hook the helpers call (`_writeComp(prop, axis, v)`).
    pub hook: String,
    /// The owner's static list of routed props (`static _comps = ["velocity"]`), stripped once read.
    pub list: String,
    /// The component names a write may target (`<owner>.<prop>.<axis>`).
    pub axes: Vec<String>,
}

impl Default for CompWrite {
    fn default() -> Self {
        CompWrite {
            write: "__compWrite".into(),
            op: "__compOp".into(),
            hook: "_writeComp".into(),
            list: "_comps".into(),
            axes: strings(&["x", "y", "z", "w"]),
        }
    }
}

/// The asset macro (`graph.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Assets {
    /// `asset("./x")` → the asset's URL string, at compile time.
    #[serde(rename = "macro")]
    pub macro_name: String,
}

impl Default for Assets {
    fn default() -> Self {
        Assets { macro_name: "asset".into() }
    }
}

/// `list` contains `name`.
pub fn has(list: &[String], name: &str) -> bool {
    list.iter().any(|s| s == name)
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

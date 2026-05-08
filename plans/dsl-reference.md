# Placement Predicate DSL — Reference

User-facing reference for the predicate language consumed by
`classic place` and `classic spawn`. **For design rationale, alternatives
considered, and crate internals see [plans/03-placement-predicates.md](03-placement-predicates.md).**

## At a glance

A *predicate* is a boolean expression over a node's hardware ad. The cluster
filters node ads by the predicate, ranks survivors with a numeric expression,
and runs your task on the best candidate.

```
any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)
```

That predicate matches any node with at least one idle NVIDIA GPU that has
≥ 80 GB of VRAM.

## Syntax

### Operator precedence (low → high)

| Level | Operators                                | Notes                                         |
|------:|------------------------------------------|-----------------------------------------------|
| 1     | `\|\|`                                   | logical or                                    |
| 2     | `&&`                                     | logical and                                   |
| 3     | `!`                                      | unary; right-binding                          |
| 4     | `==` `!=` `<` `<=` `>` `>=` `in`         | **non-associative** — `a < b < c` is an error |
| 5     | `+` `-`                                  | binary                                        |
| 6     | `*` `/`                                  |                                               |
| 7     | `-`                                      | unary                                         |
| 8     | primaries: literals, fields, `(expr)`, lists, aggregates |                               |

### Literals

| Form     | Examples                            |
|----------|-------------------------------------|
| Bool     | `true`, `false`                     |
| Int      | `32`, `0`, `-7`                     |
| HexInt   | `0x10de`, `0xFFFF`                  |
| Float    | `1.5`, `2.0e3`, `1.0E-2`            |
| String   | `"x86_64"`, `"H100\n"`              |
| List     | `[0x10de, 0x1002]`                  |

Strings support the escapes `\"`, `\\`, `\n`, `\r`, `\t`. Integers fit in
`i64`; floats are `f64`. Numeric comparisons coerce `int` → `float` when one
side is float.

### Identifiers and field paths

```
ident       := [A-Za-z_][A-Za-z0-9_]*
field_path  := ident ( "." ident )*
```

`gpu.vram_mb`, `cpu.cores`, `mem.free_mb`. The reserved keywords `any`,
`all`, `count`, `in`, `true`, `false` cannot appear as field names.

## Field reference

The DSL addresses these fields on a `NodeAd`:

| Path                                             | Type                                    |
|--------------------------------------------------|-----------------------------------------|
| `node_id`, `hostname`                            | string                                  |
| `os.kernel`, `os.distro`                         | string                                  |
| `cpu.cores`, `cpu.threads`                       | int                                     |
| `cpu.arch`, `cpu.model`                          | string (`arch` is `"x86_64"`/`"aarch64"`) |
| `mem.total_mb`, `mem.free_mb`                    | int                                     |
| `load.cpu_pct`, `load.mem_pct`, `load.load_1m`   | float (0..100 for the percents)         |
| `gen`                                            | int (per-node generation counter)       |
| `gpu`                                            | collection (use inside `any/all/count`) |
| `gpu.index`, `gpu.vendor`, `gpu.device`          | int (per element)                       |
| `gpu.model`                                      | string (per element)                    |
| `gpu.vram_mb`, `gpu.vram_free_mb`, `gpu.sm_count`| int (per element)                       |
| `gpu.in_use`, `gpu.mig`                          | bool (per element)                      |
| `pci`                                            | collection                              |
| `pci.bdf`                                        | string (e.g. `"0000:01:00.0"`)          |
| `pci.vendor`, `pci.device`, `pci.class`          | int (per element)                       |

Per-element fields (everything tagged `(per element)`) are **only valid
inside `any/all/count`**. A field not known on the relevant ad is a parse
error; a field that is known but missing on a particular node evaluates as
if absent (boolean predicates: `false`; numeric: numeric neutral element).

## Aggregations

Three aggregates iterate a collection and bind an iteration variable that
shadows the outer field path with the same name.

| Aggregate                         | Returns | Empty-collection result |
|-----------------------------------|---------|-------------------------|
| `any(coll, pred)`                 | bool    | `false`                 |
| `all(coll, pred)`                 | bool    | `true` (vacuous truth)  |
| `count(coll)`                     | int     | `0`                     |
| `count(coll, pred)`               | int     | `0`                     |

`coll` must be a known root collection (currently `gpu` or `pci`).

```text
any(gpu, gpu.vram_mb >= 80000)        # at least one ≥ 80 GB GPU
all(gpu, gpu.in_use == false)         # every GPU idle
count(gpu)                            # total GPU count
count(gpu, gpu.in_use)                # number of busy GPUs
count(pci, pci.class == 0x030000)     # display-class PCI devices
```

The two-argument form's `pred` must evaluate to bool — otherwise it's a
parse-time type error.

## `in` operator

`x in [a, b, c]` is sugar for `x == a || x == b || x == c`. Every list
element must have the same type as the LHS (checked at parse time).

```text
gpu.vendor in [0x10de, 0x1002]        # NVIDIA or AMD
cpu.arch in ["x86_64", "aarch64"]
```

## Default rank

If you don't supply a rank expression, the matcher uses:

```text
-load.cpu_pct - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))
```

Plain English: "**lower CPU load wins, and idle GPUs are massively
preferred** (each idle GPU is worth 1000 points)." Higher score wins.
Example: a node with 4 idle GPUs and 10 % CPU scores `-10 - 4000 = -4010`;
a node with 0 idle GPUs and 5 % CPU scores `-5`. The 4-idle-GPU node is
ranked higher (`-10 - 4000` is *bigger* than `-10 - 4010`? wait no). The
no-GPU node scores `-5`; the 4-idle node scores `-4010`. **Higher is
better, so `-5 > -4010` — the no-GPU node beats the 4-idle node** unless
the predicate already required a GPU. The default rank is meant for cases
where a GPU was already required by the predicate; it then picks the
loaded-least + most-idle-GPU-margin survivor.

## Examples

Six canonical predicates and their motivating workloads:

1. **Any NVIDIA GPU with ≥ 80 GB VRAM that isn't in use.** Large model
   training; user wants H100 / H200 specifically.
   ```text
   any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)
   ```

2. **Two or more idle GPUs on the same node.** Small-scale data parallel.
   ```text
   count(gpu, !gpu.in_use) >= 2
   ```

3. **Any GPU at all, but exclude MIG-partitioned ones.** Full-card workloads.
   ```text
   any(gpu, !gpu.mig && !gpu.in_use)
   ```

4. **AMD MI300X specifically.** ROCm workload.
   ```text
   any(gpu, gpu.vendor in [0x1002] && gpu.vram_mb >= 192000)
   ```

5. **At least 64 GB free RAM and CPU load below 50 %.** CPU-bound inference.
   ```text
   mem.free_mb >= 65536 && load.cpu_pct < 50.0
   ```

6. **x86_64 with ≥ 32 cores and a Mellanox NIC (vendor 0x15b3).** Tightly
   coupled networking.
   ```text
   cpu.arch == "x86_64"
     && cpu.cores >= 32
     && any(pci, pci.vendor == 0x15b3)
   ```

## Errors you might see

The format is `line N col M: <reason>` followed by the source line and a
caret pointing at the offending token. These are pinned by golden tests so
the format is stable across releases.

**Unterminated string.**
```
input:  any(gpu, gpu.model == "H100
error:  line 1 col 19: unterminated string literal
        any(gpu, gpu.model == "H100
                              ^
```

**Chained comparison.**
```
input:  cpu.cores < 32 < 64
error:  line 1 col 16: comparison operators are not associative
        cpu.cores < 32 < 64
                       ^
        hint: write `cpu.cores < 32 && 32 < 64` instead
```

**Type mismatch.**
```
input:  cpu.arch >= 5
error:  line 1 col 10: type error: cannot compare string and int
        cpu.arch >= 5
                 ^^
```

**Top-level non-bool requirement.**
```
input:  cpu.cores
error:  line 1 col 1: requirement must be a boolean expression, got int
```

**Unknown field.**
```
input:  any(gpu, gpu.warp_count >= 4)
error:  line 1 col 22: unknown field `warp_count` on GpuAd
        (known: index, vendor, device, model, vram_mb, vram_free_mb,
         sm_count, in_use, mig)
```

**Reserved keyword as field.**
```
input:  any.in == 3
error:  line 1 col 5: `in` is a reserved keyword and cannot be used
        as a field name
```

## Limits

- **Predicate text size.** Source capped at 64 KiB.
- **Eval cost.** `O(ast_size × max_collection_size)` — predicates that walk
  GPU and PCI collections in the same expression scale with both.
- **No I/O.** No file system access, no network, no shell, no time, no
  RNG. Predicates are pure functions of `(NodeAd, source)`.
- **No recursion in the language.** No closures, no user-defined functions,
  no `eval`-like indirection.
- **Tie-breaking.** When two candidates score equally, the lower
  `NodeId` byte string wins.

## Not yet supported

Tracked for v1.1 follow-up:

- **Label access syntax.** `labels["key"]` requires bracketed string
  indexing, which the v1 grammar does not include. Workaround: expose
  specific known labels as virtual fields (e.g. `labels.zone`) via the
  field resolver.
- **Custom rank tie-break strategy.** Currently ascending NodeId byte
  order; v1.1 may add a daemon-seeded random tie-break to avoid
  hot-spotting on the lowest-id node when many ties occur.
- **Smaller predicate cap.** 64 KiB is generous; may drop to 4 KiB once
  we have real-world predicates in flight.

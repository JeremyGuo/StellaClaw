# DSL Syntax

AgentFrame DSL code runs inside a short-lived restricted CPython worker. Rust
keeps the job lifecycle, budgets, output files, LLM calls, and tool calls.
Python owns normal language semantics.

The DSL is still for bounded orchestration, not arbitrary scripting. It allows
normal Python expression syntax while rejecting constructs that make execution
unbounded or unsafe.

## Execution Model

- `dsl_start` creates a background DSL job.
- The job starts a CPython worker and sends the whole code string to it.
- The worker executes the code once inside an async wrapper, so top-level
  `await` is supported.
- `emit`, LLM calls, and tool calls are JSON-RPC callbacks from Python to Rust.
- Interrupting `dsl_start` or `dsl_wait` only interrupts the outer wait. The DSL
  job continues until it finishes, fails, times out internally, or is killed by
  `dsl_kill`.
- The runtime enforces budgets for total runtime, LLM calls, tool calls, emit
  calls, code size, and returned output size.

## Available Globals

```python
emit(text)
quit()
quit(value)

handle = LLM()

handle.system("system prompt")
handle.config(temperature=0.2)
forked = handle.fork()

text = await handle.gen("prompt")
text = await handle.gen("Hello {name}", name="Ada")
data = await handle.json("Return an object")
choice = await handle.select("Choose", ["A", "B", "C"])

result = await tool({"name": "tool_name", "args": {"arg": value}})
```

Notes:

- `LLM` is only a callable handle factory.
- Use `LLM()` without arguments. DSL LLM calls always use the same model as the
  `dsl_start` caller.
- Do not use `LLM(model=...)` or `handle.config(model=...)`; model switching is
  intentionally not available inside DSL.
- External tools are called with the global `tool(...)` function.
- Do not call `LLM.llm()`, `LLM.tool()`, `LLM.gen()`, `LLM.json()`, or
  `LLM.select()`.
- `handle.select` accepts choices only as the second positional list argument:
  `await handle.select("prompt", ["red", "blue"])`.
- `handle.gen` and `handle.json` accept Python `str.format` keyword values.
- `tool(...)` accepts exactly one dict argument with `name` and optional `args`.
  Do not call `tool("tool_name", arg=value)`.

## Supported Python

Because the code runs in CPython, normal expression semantics work:

```python
x = 1 + 1
text = f"value={x}"
items = [1, 2, 3, 4]
obj = {"name": "Ada", "scores": items[:2]}
label = "many" if len(items) > 2 else "few"
upper = obj["name"].upper()
emit(f"{label}:{upper}:{items[1:3]}:{'!' * 3}")
```

Allowed statement shapes include:

```python
x = expression
if condition:
    ...
else:
    ...
emit(expression)
quit(expression)
await handle.gen(...)
result = await tool({"name": "tool_name", "args": {...}})
```

Safe builtins exposed to DSL code:

```python
abs all any bool dict enumerate float int len list max min range round sorted str sum tuple type zip
```

The worker does not expose `open`, `eval`, `exec`, `compile`, `globals`,
`locals`, `vars`, `getattr`, `setattr`, `delattr`, `dir`, `input`,
`breakpoint`, or `__import__`.

## Runtime Functions

### `emit`

```python
emit("hello")
emit(f"count={count}")
```

Appends visible DSL output. If the script finishes without `quit(value)`, the
final result is all emitted text joined with newlines. If nothing was emitted,
the default result is `0`.

### `quit`

```python
quit()
quit({"ok": True})
```

Completes the DSL job immediately. `quit()` returns the default result.
`quit(value)` returns the explicit value.

### `LLM`

```python
handle = LLM()
```

Creates an LLM handle for the same model that called `dsl_start`.

### LLM Handle Methods

```python
handle.system("You are concise.")
handle.config(temperature=0.2)
forked = handle.fork()
text = await handle.gen("Write one sentence.")
obj = await handle.json("Return JSON only.")
choice = await handle.select("Pick one", ["red", "blue"])
```

Notes:

- `handle.system` appends a system message to that handle.
- `handle.config` stores extra upstream payload fields for that handle.
  `model=...` is not allowed.
- `handle.fork` clones the current handle history/config into a new handle.
- `handle.gen` returns text.
- `handle.json` parses the returned text as JSON.
- `handle.select` requires the returned text to exactly match one choice.

### `tool`

```python
content = await tool({"name": "file_read", "args": {"path": "README.md"}})
emit(content)
```

Calls a normal AgentFrame tool and returns the parsed JSON result when possible,
otherwise a string.

Restrictions:

- DSL lifecycle tools cannot be called recursively through `tool`, for example
  `tool({"name": "dsl_start", "args": {...}})` is rejected.
- Tool behavior must preserve the normal tool registry permissions, sandboxing,
  remote/workpath behavior, lifecycle semantics, and output limits.

## Forbidden Syntax

The worker rejects these forms before execution:

```python
for x in xs:
while True:
async for x in xs:
def f():
async def f():
class C:
lambda x: x
import os
from x import y
try:
with ctx:
async with ctx:
return x
yield x
global x
nonlocal x
del x
[x for x in xs]
{x: x for x in xs}
(x for x in xs)
```

Also forbidden:

- names starting with `_`
- attribute access where the attribute starts with `_`
- recursive DSL lifecycle calls through `tool({"name": "dsl_*", ...})`
- direct file or process access through unsafe builtins

## Common Examples

Summarize a file:

```python
content = await tool({"name": "file_read", "args": {"path": "README.md"}})
handle = LLM()
summary = await handle.gen("Summarize this briefly:\n{content}", content=content)
emit(summary)
```

Choose from alternatives:

```python
handle = LLM()
choice = await handle.select("Choose the best color", ["red", "blue", "green"])
emit(choice)
```

Use Python branching:

```python
data = await tool({"name": "file_read", "args": {"path": "README.md"}})
if "install" in data["content"].lower():
    emit("README mentions install")
else:
    emit("README does not mention install")
```

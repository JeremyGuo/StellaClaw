from __future__ import annotations

import ast
import asyncio
import copy
import json
import sys
import traceback
from typing import Any


_next_id = 1
_emitted: list[str] = []
_DEFAULT = object()


class DslQuit(Exception):
    def __init__(self, value: Any = _DEFAULT):
        self.value = default_result() if value is _DEFAULT else value


def send_message(message: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(message, ensure_ascii=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def recv_message() -> dict[str, Any]:
    line = sys.stdin.readline()
    if not line:
        raise RuntimeError("DSL worker input stream closed")
    value = json.loads(line)
    if not isinstance(value, dict):
        raise RuntimeError("DSL worker expected a JSON object")
    return value


def rpc_call(method: str, params: dict[str, Any]) -> Any:
    global _next_id
    request_id = _next_id
    _next_id += 1
    send_message({"id": request_id, "method": method, "params": params})
    while True:
        response = recv_message()
        if response.get("id") != request_id:
            raise RuntimeError(f"unexpected RPC response id: {response.get('id')!r}")
        if "error" in response:
            raise RuntimeError(str(response["error"]))
        return response.get("result")


def to_jsonable(value: Any) -> Any:
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if isinstance(value, (list, tuple)):
        return [to_jsonable(item) for item in value]
    if isinstance(value, dict):
        return {str(key): to_jsonable(item) for key, item in value.items()}
    return str(value)


def default_result() -> Any:
    if _emitted:
        return "\n".join(_emitted)
    return 0


def emit(text: Any) -> None:
    rendered = str(text)
    rpc_call("emit", {"text": rendered})
    _emitted.append(rendered)


def quit(value: Any = _DEFAULT) -> None:
    raise DslQuit(value)


def parse_json_response(text: str) -> Any:
    trimmed = text.strip()
    if trimmed.startswith("```json") and trimmed.endswith("```"):
        trimmed = trimmed[len("```json") : -len("```")].strip()
    elif trimmed.startswith("```") and trimmed.endswith("```"):
        trimmed = trimmed[len("```") : -len("```")].strip()
    return json.loads(trimmed)


def render_prompt(prompt: Any, variables: dict[str, Any]) -> str:
    text = str(prompt)
    if not variables:
        return text
    return text.format(**variables)


class LLMHandle:
    def __init__(self):
        self.messages: list[dict[str, str]] = []
        self.extra_payload: dict[str, Any] = {}

    def system(self, text: Any) -> None:
        self.messages.append({"role": "system", "content": str(text)})

    def config(self, **kwargs: Any) -> None:
        if "model" in kwargs:
            raise RuntimeError("DSL only supports LLM(); model switching is not allowed")
        self.extra_payload.update(kwargs)

    def fork(self) -> "LLMHandle":
        return copy.deepcopy(self)

    async def gen(self, prompt: Any, **variables: Any) -> str:
        rendered = render_prompt(prompt, variables)
        result = rpc_call(
            "llm_call",
            {
                "messages": self.messages,
                "extra_payload": self.extra_payload,
                "prompt": rendered,
            },
        )
        text = str(result.get("text", ""))
        self.messages.append({"role": "user", "content": rendered})
        self.messages.append({"role": "assistant", "content": text})
        return text

    async def json(self, prompt: Any, **variables: Any) -> Any:
        text = await self.gen(str(render_prompt(prompt, variables)) + "\n\nReturn JSON only.")
        return parse_json_response(text)

    async def select(self, prompt: Any, choices: list[str]) -> str:
        if not isinstance(choices, list) or not choices or not all(
            isinstance(choice, str) for choice in choices
        ):
            raise RuntimeError("handle.select requires choices as a non-empty list of strings")
        text = await self.gen(
            str(prompt)
            + "\n\nReturn exactly one of these choices and nothing else: "
            + ", ".join(choices)
        )
        selected = text.strip()
        if selected not in choices:
            raise RuntimeError(f"select returned {selected!r}, not one of {choices!r}")
        return selected


def LLM() -> LLMHandle:
    return LLMHandle()


async def tool(request: dict[str, Any]) -> Any:
    if not isinstance(request, dict):
        raise RuntimeError(
            "tool requires a dict request: await tool({'name': 'tool_name', 'args': {...}})"
        )
    name = request.get("name")
    if not isinstance(name, str):
        raise RuntimeError("tool request requires string field 'name'")
    args = request.get("args", {})
    if not isinstance(args, dict):
        raise RuntimeError("tool request field 'args' must be a dict when provided")
    return rpc_call("tool_call", {"name": name, "args": to_jsonable(args)})


FORBIDDEN_NODES = (
    ast.For,
    ast.AsyncFor,
    ast.While,
    ast.ListComp,
    ast.SetComp,
    ast.DictComp,
    ast.GeneratorExp,
    ast.Import,
    ast.ImportFrom,
    ast.FunctionDef,
    ast.AsyncFunctionDef,
    ast.ClassDef,
    ast.Lambda,
    ast.With,
    ast.AsyncWith,
    ast.Try,
    ast.Global,
    ast.Nonlocal,
    ast.Yield,
    ast.YieldFrom,
    ast.Delete,
    ast.Return,
)


FORBIDDEN_BUILTIN_CALLS = {
    "__import__",
    "breakpoint",
    "compile",
    "delattr",
    "dir",
    "eval",
    "exec",
    "getattr",
    "globals",
    "input",
    "locals",
    "open",
    "setattr",
    "vars",
}


def validate_code(code: str) -> None:
    tree = ast.parse(code, mode="exec")
    for node in ast.walk(tree):
        if isinstance(node, FORBIDDEN_NODES):
            raise SyntaxError(f"unsupported DSL syntax: {type(node).__name__}")
        if isinstance(node, ast.Name) and node.id.startswith("_"):
            raise SyntaxError("DSL names starting with '_' are not allowed")
        if isinstance(node, ast.Attribute) and node.attr.startswith("_"):
            raise SyntaxError("DSL attributes starting with '_' are not allowed")
        if isinstance(node, ast.Call):
            if isinstance(node.func, ast.Name) and node.func.id in FORBIDDEN_BUILTIN_CALLS:
                raise SyntaxError(f"unsupported DSL function: {node.func.id}")
            if isinstance(node.func, ast.Name) and node.func.id == "tool":
                if node.keywords or len(node.args) != 1:
                    raise SyntaxError(
                        "tool() requires a single dict argument: tool({'name': 'tool_name', 'args': {...}})"
                    )
                if isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str):
                    raise SyntaxError(
                        "tool() no longer accepts name plus kwargs; pass a dict request"
                    )
                if isinstance(node.args[0], ast.Dict):
                    for key, value in zip(node.args[0].keys, node.args[0].values):
                        if (
                            isinstance(key, ast.Constant)
                            and key.value == "name"
                            and isinstance(value, ast.Constant)
                            and isinstance(value.value, str)
                            and value.value.startswith("dsl_")
                        ):
                            raise SyntaxError("DSL cannot recursively call DSL lifecycle tools")


def async_wrapper_source(code: str) -> str:
    lines = code.splitlines()
    if not lines:
        return "async def __dsl_main__():\n    pass\n"
    body = "\n".join(("    " + line if line.strip() else "") for line in lines)
    return "async def __dsl_main__():\n" + body + "\n"


def safe_globals() -> dict[str, Any]:
    return {
        "__builtins__": {
            "abs": abs,
            "all": all,
            "any": any,
            "bool": bool,
            "dict": dict,
            "enumerate": enumerate,
            "float": float,
            "int": int,
            "len": len,
            "list": list,
            "max": max,
            "min": min,
            "range": range,
            "round": round,
            "sorted": sorted,
            "str": str,
            "sum": sum,
            "tuple": tuple,
            "type": type,
            "zip": zip,
        },
        "emit": emit,
        "quit": quit,
        "LLM": LLM,
        "tool": tool,
        "True": True,
        "False": False,
        "None": None,
    }


async def execute_code(code: str) -> Any:
    validate_code(code)
    namespace = safe_globals()
    compiled = compile(async_wrapper_source(code), "<dsl>", "exec")
    exec(compiled, namespace)
    main = namespace["__dsl_main__"]
    result = await main()
    return default_result() if result is None else result


def handle_exec(request_id: Any, params: dict[str, Any]) -> None:
    code = str(params.get("code", ""))
    try:
        result = asyncio.run(execute_code(code))
        send_message({"id": request_id, "result": {"ok": True, "result": to_jsonable(result)}})
    except DslQuit as done:
        send_message({"id": request_id, "result": {"ok": True, "result": to_jsonable(done.value)}})
    except BaseException as error:
        send_message(
            {
                "id": request_id,
                "result": {
                    "ok": False,
                    "error": f"{type(error).__name__}: {error}",
                    "traceback": traceback.format_exc(),
                },
            }
        )


def main() -> None:
    request = recv_message()
    method = request.get("method")
    request_id = request.get("id")
    params = request.get("params") or {}
    if method != "exec":
        send_message({"id": request_id, "error": f"unknown method: {method}"})
        return
    if not isinstance(params, dict):
        send_message({"id": request_id, "error": "exec params must be an object"})
        return
    handle_exec(request_id, params)


if __name__ == "__main__":
    main()

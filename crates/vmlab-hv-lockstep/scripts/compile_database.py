"""Read the command and optional output fields of a compilation database."""
import pathlib
import shlex


def compile_command(entry):
    if "arguments" in entry:
        return list(entry["arguments"])
    return shlex.split(entry["command"])


def object_output(entry):
    if "output" in entry:
        return entry["output"]
    command = compile_command(entry)
    try:
        return command[command.index("-o") + 1]
    except (ValueError, IndexError) as error:
        raise ValueError(f"compile command for {entry['file']} has no object output") from error


def select_entries(entries):
    selected = []
    for entry in entries:
        name = pathlib.Path(entry["file"]).name
        output = pathlib.Path(object_output(entry))
        if (name in {"sail_riscv_model.cpp", "riscv_platform_if.cpp", "riscv_softfloat.cpp"}
                or "softfloat.dir" in output.parts or "sail_runtime" in output.parts
                or "sail_runtime.dir" in output.parts):
            selected.append(entry)
    return selected

"""The stub `foundation/_native.pyi` states the surface of the module."""

import ast
import collections.abc
import inspect
import os
import pathlib
import types
import typing
import unittest

import foundation
from foundation import _native

STUB = pathlib.Path(foundation.__file__).with_name("_native.pyi")

# Set on each class that a Python class can subclass.
BASETYPE = 1 << 10


def stub() -> types.ModuleType:
    """The module that the stub defines."""
    module = types.ModuleType("_native_stub")
    exec(compile(STUB.read_text(), str(STUB), "exec"), vars(module))
    return module


def conforms(value: object, hint: typing.Any) -> bool:
    """Whether `value` is of the type `hint`, with each item of a `dict`, a `tuple`, and
    a `list`, `tuple`, or `set` for an `Iterable`, the path of an `os.PathLike`, and the
    value of an alias of a `type` statement, such as `NDArray` of numpy 2.5."""
    origin, args = typing.get_origin(hint), typing.get_args(hint)
    if hint is typing.Any:
        return True
    if hint is None or hint is types.NoneType:
        return value is None
    if hasattr(origin, "__value__"):
        return conforms(value, origin.__value__)
    if origin in (typing.Union, types.UnionType):
        return any(conforms(value, arg) for arg in args)
    if origin is typing.Literal:
        return any(type(value) is type(arg) and value == arg for arg in args)
    if origin is dict:
        return isinstance(value, dict) and all(
            conforms(k, args[0]) and conforms(v, args[1]) for k, v in value.items()
        )
    if origin is tuple:
        if not isinstance(value, tuple):
            return False
        if args[-1] is Ellipsis:
            return all(conforms(item, args[0]) for item in value)
        return len(value) == len(args) and all(map(conforms, value, args))
    if origin is collections.abc.Iterable:
        # Another iterable, such as a generator, is not read: a read consumes it.
        items = value if isinstance(value, (list, tuple, set, frozenset)) else []
        return isinstance(value, origin) and all(conforms(i, args[0]) for i in items)
    if origin is os.PathLike:
        return isinstance(value, origin) and conforms(os.fspath(value), args[0])
    return isinstance(value, origin or hint)


def stated() -> tuple[types.ModuleType, dict[str, typing.Any]]:
    """The module of the stub, and the namespace in which its annotations name the
    classes of the module, not of the stub."""
    module = stub()
    names = vars(module) | {n: c for n, c in vars(_native).items() if type(c) is type}
    return module, names


def called(
    test: unittest.TestCase, function: typing.Any, *args: object, **kwargs: object
) -> typing.Any:
    """Calls `function` of the module (such as `client.open_writer`) with `args` and
    `kwargs`, and returns what it gives. Asserts that the function of the stub of the
    same name takes each argument, that it states the type of the value, and, as
    `assert_stated` does, the type of each attribute of the value."""
    module, names = stated()
    by = function.__qualname__
    owner, _, name = by.rpartition(".")
    declared = getattr(getattr(module, owner) if owner else module, name)
    signature = inspect.signature(declared)
    if owner:
        signature = signature.replace(parameters=[*signature.parameters.values()][1:])
    hints = typing.get_type_hints(declared, names)
    for parameter, value in signature.bind(*args, **kwargs).arguments.items():
        hint = hints[parameter]
        test.assertTrue(conforms(value, hint), f"{by} takes {value!r}, not {hint}")
    value = function(*args, **kwargs)
    hint = names[owner] if hints["return"] is typing.Self else hints["return"]
    test.assertTrue(conforms(value, hint), f"{by} gave {value!r}, not {hint}")
    if hasattr(module, type(value).__name__):
        assert_stated(test, value)
    return value


def assert_stated(test: unittest.TestCase, value: object) -> None:
    """Asserts that `value` is of each type that the stub states for each attribute
    and property of its class."""
    module, names = stated()
    kind = getattr(module, type(value).__name__)
    hints = typing.get_type_hints(kind, names)
    test.assertEqual(sorted(getattr(value, "__dict__", {})), sorted(hints))
    for name, member in vars(kind).items():
        if isinstance(member, property):
            hints[name] = typing.get_type_hints(member.fget, names)["return"]
    test.assertTrue(hints, f"the stub states no attribute of {type(value).__name__}")
    for name, hint in hints.items():
        found = getattr(value, name)
        test.assertTrue(conforms(found, hint), f"{name} is {found!r}, not {hint}")


def registered(stub: type, module: type) -> list[type]:
    """Each base of the class `stub` of the stub that the class `module` of the module
    is not built on: an ABC that the module registers it with, which a type checker
    reads only from the stub."""
    return [base for base in stub.__bases__ if base not in module.__mro__]


def public(name: str) -> bool:
    """Whether a program calls the item `name`: a name with no `_` first, or a special
    method."""
    return not name.startswith("_") or (name.startswith("__") and name.endswith("__"))


def defined(body: list[ast.stmt]) -> list[str]:
    """The names of the public functions and classes that `body` defines."""
    kinds = (ast.FunctionDef, ast.ClassDef)
    return sorted(s.name for s in body if isinstance(s, kinds) and public(s.name))


def built(namespace: typing.Mapping[str, object]) -> dict[str, object]:
    """The public items of `namespace` of the module, with each special method that it
    defines and no other special attribute."""
    return {
        name: item
        for name, item in namespace.items()
        if public(name) and (not name.startswith("_") or callable(item))
    }


def doc(item: object) -> str | None:
    """The doc of `item` with each run of white space as one space."""
    text = inspect.getdoc(item)
    return None if text is None else " ".join(text.split())


def parameters(function: typing.Any, method: bool) -> list[tuple[str, object, object]]:
    """The name, kind, and default of each parameter of `function`, with no `self` for a
    method."""
    found = list(inspect.signature(function).parameters.values())
    return [(p.name, p.kind, p.default) for p in found[1 if method else 0 :]]


class StubTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tree = ast.parse(STUB.read_text(), str(STUB))
        self.stub = stub()
        self.classes = {
            s.name: s for s in self.tree.body if isinstance(s, ast.ClassDef)
        }

    def test_the_package_is_typed(self) -> None:
        self.assertTrue(STUB.with_name("py.typed").is_file())

    def test_the_stub_defines_each_item_of_the_module(self) -> None:
        self.assertEqual(defined(self.tree.body), sorted(built(vars(_native))))

    def test_each_function_has_the_signature_and_doc_of_the_module(self) -> None:
        for name, item in built(vars(_native)).items():
            if not isinstance(item, type):
                with self.subTest(name):
                    self.check(getattr(self.stub, name), item, method=False)

    def test_each_class_has_the_bases_doc_and_members_of_the_module(self) -> None:
        for name, item in built(vars(_native)).items():
            if isinstance(item, type):
                with self.subTest(name):
                    self.check_class(name, item)

    def test_only_frame_has_a_base_that_the_module_registers(self) -> None:
        found = {
            name: [base.__name__ for base in registered(getattr(self.stub, name), item)]
            for name, item in built(vars(_native)).items()
            if isinstance(item, type) and registered(getattr(self.stub, name), item)
        }
        self.assertEqual(found, {"Frame": ["Mapping"]})

    def test_each_annotation_of_the_stub_names_a_type(self) -> None:
        for name in defined(self.tree.body):
            item = getattr(self.stub, name)
            members = [item]
            if isinstance(item, type):
                members += [vars(item)[n] for n in defined(self.classes[name].body)]
            for member in members:
                with self.subTest(name, member=getattr(member, "__name__", None)):
                    hinted = getattr(member, "fget", member)
                    typing.get_type_hints(hinted, vars(self.stub))

    def check_class(self, name: str, module: type) -> None:
        stub = getattr(self.stub, name)
        added = registered(stub, module)
        for base in added:
            with self.subTest(base=base.__name__):
                # Not by a hook of the ABC that reads the methods of the class.
                self.assertIs(base.__subclasshook__(module), NotImplemented)
                self.assertTrue(issubclass(module, base))
        stated = [base.__name__ for base in stub.__bases__ if base not in added]
        bases = [base.__name__ for base in module.__bases__]
        self.assertEqual(stated or ["object"], bases)
        final = not module.__flags__ & BASETYPE
        self.assertEqual(getattr(stub, "__final__", False), final)
        self.assertEqual(doc(stub), doc(module))
        members = built(vars(module))
        self.assertEqual(defined(self.classes[name].body), sorted(members))
        for member, item in members.items():
            with self.subTest(member=member):
                stated = vars(stub)[member]
                if inspect.isgetsetdescriptor(item):
                    self.assertIsInstance(stated, property)
                    self.assertEqual(doc(stated), doc(item))
                else:
                    self.check(stated, item, method=True)
        # An attribute that the module sets on each exception that it raises, which
        # the doc of the class names.
        for attribute in stub.__annotations__:
            with self.subTest(attribute=attribute):
                self.assertIn(f"`{attribute}`", module.__doc__ or "")

    def check(self, stub: typing.Any, module: typing.Any, method: bool) -> None:
        self.assertEqual(parameters(stub, method), parameters(module, method))
        if isinstance(module, types.WrapperDescriptorType):
            # Python gives the doc of a slot, which the module cannot set and which
            # changes between versions of Python.
            self.assertIsNone(doc(stub))
        else:
            self.assertEqual(doc(stub), doc(module))

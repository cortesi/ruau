use ruau_syntax::{
    Expr, Type,
    parse::parse,
    visit::{Visitor, WalkControl, walk_stat},
};

use super::*;

#[derive(Default)]
struct SelectedIds {
    function_ids: Vec<SyntaxId>,
    type_ids: Vec<SyntaxId>,
}

#[derive(Default)]
struct CallIds {
    ids: Vec<SyntaxId>,
}

impl<'ast> Visitor<'ast> for SelectedIds {
    fn visit_expr(&mut self, expr: &'ast Expr) -> WalkControl {
        if matches!(expr, Expr::Function { .. }) {
            self.function_ids.push(expr.syntax_id());
        }
        WalkControl::Continue
    }

    fn visit_type(&mut self, luau_type: &'ast Type) -> WalkControl {
        self.type_ids.push(luau_type.syntax_id());
        WalkControl::Continue
    }
}

impl<'ast> Visitor<'ast> for CallIds {
    fn visit_expr(&mut self, expr: &'ast Expr) -> WalkControl {
        if let Expr::Call { syntax_id, .. } = expr {
            self.ids.push(*syntax_id);
        }
        WalkControl::Continue
    }
}

fn parse_root(source: &str) -> Stat {
    let parse = parse(source);
    assert!(parse.errors.is_empty(), "{:?}", parse.errors);
    parse.root
}

fn call_ids(root: &Stat) -> Vec<SyntaxId> {
    let mut calls = CallIds::default();
    walk_stat(root, &mut calls);
    calls.ids
}

fn function_registry_order(functions: &FunctionRegistry) -> Vec<SyntaxId> {
    functions
        .ordered_ids()
        .iter()
        .map(|id| id.syntax_id())
        .collect()
}

fn ordered_function_infos(functions: &FunctionRegistry) -> Vec<&FunctionInfo> {
    functions
        .ordered_ids()
        .iter()
        .map(|id| {
            functions
                .get(*id)
                .expect("ordered function id is registered")
        })
        .collect()
}

#[test]
fn module_identity_maps_are_keyed_by_parser_syntax_ids() {
    let root = parse_root(
        r#"
local function outer(x: number)
    local inner = function(y: string)
        return x + 1
    end
    return inner
end
"#,
    );

    let mut selected = SelectedIds::default();
    walk_stat(&root, &mut selected);
    let (analysis, functions) =
        collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.expression_count() > selected.function_ids.len());
    assert_eq!(analysis.type_count(), selected.type_ids.len());
    assert!(analysis.local_count() >= 3);
    for id in selected.function_ids.iter().copied() {
        assert!(analysis.contains_expression(id));
    }
    for id in selected.type_ids.iter().copied() {
        assert!(analysis.contains_type(id));
    }
    assert_eq!(functions.len(), selected.function_ids.len());
    for id in selected.function_ids {
        let function_id = FunctionId::new(id);
        assert!(functions.get(function_id).is_some());
    }
}

#[test]
fn function_registry_orders_nested_functions_before_parents() {
    let root = parse_root(
        r#"
local function outer()
    local function inner()
        return function()
        end
    end
end
"#,
    );

    let mut preorder = SelectedIds::default();
    walk_stat(&root, &mut preorder);
    let (_, functions) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    let expected = preorder.function_ids.into_iter().rev().collect::<Vec<_>>();
    assert_eq!(function_registry_order(&functions), expected);
    for (index, id) in functions.ordered_ids().iter().copied().enumerate() {
        assert_eq!(
            functions.get(id).map(FunctionInfo::compile_order),
            Some(index)
        );
    }
}

#[test]
fn function_registry_records_static_and_return_count_facts() {
    let root = parse_root(
        r#"
local function typed_one(arg: number): number
    return 1
end

local function call_multret()
    return typed_one()
end

local function vararg_one(...)
    return 1
end
"#,
    );

    let (_, functions) = collect_module_identities(&root, &UpstreamCompilerOptions::default());
    let infos = ordered_function_infos(&functions);

    assert_eq!(infos.len(), 3);
    assert_eq!(infos[0].debug_name(), "typed_one");
    assert_eq!(infos[0].arg_count(), 1);
    assert_eq!(infos[0].function_depth(), 1);
    assert!(infos[0].has_type_annotations());
    assert!(infos[0].syntactic_inline_candidate());
    assert!(infos[0].returns_one());

    assert_eq!(infos[1].debug_name(), "call_multret");
    assert!(!infos[1].returns_one());

    assert_eq!(infos[2].debug_name(), "vararg_one");
    assert!(infos[2].vararg());
    assert!(!infos[2].syntactic_inline_candidate());
    assert!(infos[2].returns_one());
}

#[test]
fn function_registry_records_direct_and_forwarded_upvalues() {
    let root = parse_root(
        r#"
local top = 1
local rewritten = 2

local function outer(arg)
    local copy = rewritten
    rewritten = arg
    local local_only = copy

    local function inner()
        return top + rewritten + local_only
    end

    return function()
        return top
    end
end
"#,
    );

    let (_, functions) = collect_module_identities(&root, &UpstreamCompilerOptions::default());
    let infos = ordered_function_infos(&functions);
    let upvalue_names = |index: usize| {
        infos[index]
            .upvalues()
            .iter()
            .map(FunctionUpvalueInfo::name)
            .collect::<Vec<_>>()
    };

    assert_eq!(infos.len(), 3);
    assert_eq!(infos[0].debug_name(), "inner");
    assert_eq!(upvalue_names(0), vec!["top", "rewritten", "local_only"]);
    assert!(!infos[0].upvalues()[0].is_written());
    assert!(infos[0].upvalues()[1].is_written());
    assert_eq!(infos[0].upvalues()[2].function_depth(), 1);

    assert_eq!(infos[1].debug_name(), "");
    assert_eq!(upvalue_names(1), vec!["top"]);

    assert_eq!(infos[2].debug_name(), "outer");
    assert_eq!(upvalue_names(2), vec!["rewritten", "top"]);
    assert!(infos[2].upvalues()[0].is_written());
    assert_eq!(infos[2].upvalues()[1].function_depth(), 0);
}

#[test]
fn value_tracking_records_local_initializers_and_writes() {
    let root = parse_root(
        r#"
local immutable = 1
local rewritten = 2
local empty
rewritten = 3
local function outer(arg)
    arg = immutable
    empty = arg
end
"#,
    );

    let Stat::Block { body, .. } = &root else {
        panic!("expected block root");
    };
    let Stat::Local {
        vars: immutable_vars,
        values: immutable_values,
        ..
    } = &body[0]
    else {
        panic!("expected immutable local");
    };
    let immutable = immutable_vars[0].id;
    let immutable_init = immutable_values[0].syntax_id();
    let Stat::Local {
        vars: rewritten_vars,
        values: rewritten_values,
        ..
    } = &body[1]
    else {
        panic!("expected rewritten local");
    };
    let rewritten = rewritten_vars[0].id;
    let rewritten_init = rewritten_values[0].syntax_id();
    let Stat::Local {
        vars: empty_vars, ..
    } = &body[2]
    else {
        panic!("expected empty local");
    };
    let empty = empty_vars[0].id;
    let Stat::LocalFunction {
        name: outer,
        func: outer_func,
        ..
    } = &body[4]
    else {
        panic!("expected local function");
    };
    let Expr::Function { args, .. } = outer_func.as_ref() else {
        panic!("expected function expression");
    };
    let arg = args[0].id;

    let (mut analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert_eq!(
        analysis
            .variable(immutable)
            .and_then(VariableFact::initial_expr),
        Some(immutable_init)
    );
    assert!(
        !analysis
            .variable(immutable)
            .is_some_and(VariableFact::is_written)
    );
    assert_eq!(
        analysis
            .variable(rewritten)
            .and_then(VariableFact::initial_expr),
        Some(rewritten_init)
    );
    assert!(
        analysis
            .variable(rewritten)
            .is_some_and(VariableFact::is_written)
    );
    assert_eq!(
        analysis
            .variable(empty)
            .and_then(VariableFact::initial_expr),
        None
    );
    assert!(
        analysis
            .variable(empty)
            .is_some_and(VariableFact::is_written)
    );
    assert_eq!(
        analysis
            .variable(outer.id)
            .and_then(VariableFact::initial_expr),
        Some(outer_func.syntax_id())
    );
    assert!(!analysis.variable(outer.id).unwrap().is_constant());
    analysis.mark_local_constant(outer.id, true);
    assert!(analysis.variable(outer.id).unwrap().is_constant());
    assert!(
        !analysis
            .variable(outer.id)
            .is_some_and(VariableFact::is_written)
    );
    assert_eq!(
        analysis.variable(arg).and_then(VariableFact::initial_expr),
        None
    );
    assert!(analysis.variable(arg).is_some_and(VariableFact::is_written));
}

#[test]
fn value_tracking_records_global_states_and_fenv_use() {
    let root = parse_root(
        r#"
foo = getfenv
local t = {}
t[function()
    bar = setfenv
end] = 3
"#,
    );

    let options = UpstreamCompilerOptions {
        mutable_globals: vec![String::from("mutable")],
        ..UpstreamCompilerOptions::default()
    };
    let (analysis, _) = collect_module_identities(&root, &options);

    assert_eq!(analysis.global_state("_G"), GlobalState::Mutable);
    assert_eq!(analysis.global_state("mutable"), GlobalState::Mutable);
    assert_eq!(analysis.global_state("foo"), GlobalState::Written);
    assert_eq!(analysis.global_state("bar"), GlobalState::Written);
    assert_eq!(analysis.global_state("getfenv"), GlobalState::Default);
    assert_eq!(analysis.global_state("setfenv"), GlobalState::Default);
    assert_eq!(
        analysis.globals_blocking_imports(),
        ["_G", "bar", "foo", "mutable"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    assert!(analysis.getfenv_used());
    assert!(analysis.setfenv_used());
}

#[test]
fn builtin_analysis_records_direct_global_member_calls() {
    let root = parse_root("return math.abs(-1)");
    let calls = call_ids(&root);
    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    let builtin = analysis
        .builtin_call(calls[0])
        .expect("math.abs call is builtin");
    assert_eq!(builtin.function_id(), 2);
    assert_eq!(builtin.path(), ["math", "abs"]);
}

#[test]
fn builtin_analysis_resolves_immutable_local_library_aliases() {
    let root = parse_root(
        r#"
local m = math
return m.abs(-1)
"#,
    );
    let calls = call_ids(&root);
    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    let builtin = analysis
        .builtin_call(calls[0])
        .expect("alias call is builtin");
    assert_eq!(builtin.function_id(), 2);
    assert_eq!(builtin.path(), ["math", "abs"]);
}

#[test]
fn builtin_analysis_rejects_written_local_library_aliases() {
    let root = parse_root(
        r#"
local m = math
m = replacement
return m.abs(-1)
"#,
    );
    let calls = call_ids(&root);
    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.builtin_call(calls[0]).is_none());
}

#[test]
fn builtin_analysis_accepts_safe_env_or_aliases() {
    let root = parse_root(
        r#"
local m = math or replacement
return m.abs(-1)
"#,
    );
    let calls = call_ids(&root);
    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    let builtin = analysis
        .builtin_call(calls[0])
        .expect("safe-env alias call is builtin");
    assert_eq!(builtin.function_id(), 2);
    assert_eq!(builtin.path(), ["math", "abs"]);
}

#[test]
fn builtin_analysis_applies_select_vararg_eligibility() {
    let root = parse_root("return select(1, value), select(1, ...)");
    let calls = call_ids(&root);
    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.builtin_call(calls[0]).is_none());
    assert_eq!(analysis.builtin_call(calls[1]).unwrap().function_id(), 57);
}

#[test]
fn builtin_analysis_applies_disabled_builtins() {
    let root = parse_root("return math.abs(-1), math.max(1, 2)");
    let calls = call_ids(&root);
    let options = UpstreamCompilerOptions {
        disabled_builtins: vec![String::from("math.abs")],
        ..UpstreamCompilerOptions::default()
    };
    let (analysis, _) = collect_module_identities(&root, &options);

    assert!(analysis.builtin_call(calls[0]).is_none());
    assert_eq!(analysis.builtin_call(calls[1]).unwrap().function_id(), 18);
}

#[test]
fn builtin_analysis_respects_mutable_globals() {
    let root = parse_root("return math.abs(-1)");
    let calls = call_ids(&root);
    let options = UpstreamCompilerOptions {
        mutable_globals: vec![String::from("math")],
        ..UpstreamCompilerOptions::default()
    };
    let (analysis, _) = collect_module_identities(&root, &options);

    assert!(analysis.builtin_call(calls[0]).is_none());
}

#[test]
fn constant_analysis_records_expression_and_local_constants() {
    let root = parse_root(
        r#"
local a = 1 + 2 * 3
local b = a
local c
return b, c
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local {
        vars: a_vars,
        values: a_values,
        ..
    } = &body[0]
    else {
        panic!("expected first local");
    };
    let Stat::Local {
        vars: b_vars,
        values: b_values,
        ..
    } = &body[1]
    else {
        panic!("expected second local");
    };
    let Stat::Local { vars: c_vars, .. } = &body[2] else {
        panic!("expected third local");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert_eq!(
        analysis.constant_expr(a_values[0].syntax_id()),
        Some(&ConstantValue::Number(7.0))
    );
    assert_eq!(
        analysis.local_constant(a_vars[0].id),
        Some(&ConstantValue::Number(7.0))
    );
    assert_eq!(
        analysis.constant_expr(b_values[0].syntax_id()),
        Some(&ConstantValue::Number(7.0))
    );
    assert_eq!(
        analysis.local_constant(b_vars[0].id),
        Some(&ConstantValue::Number(7.0))
    );
    assert_eq!(
        analysis.local_constant(c_vars[0].id),
        Some(&ConstantValue::Nil)
    );
    assert!(analysis.variable(a_vars[0].id).unwrap().is_constant());
    assert!(analysis.variable(b_vars[0].id).unwrap().is_constant());
    assert!(analysis.variable(c_vars[0].id).unwrap().is_constant());
}

#[test]
fn constant_analysis_rejects_written_local_constants() {
    let root = parse_root(
        r#"
local a = 1
a = 2
return a
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { vars, .. } = &body[0] else {
        panic!("expected local");
    };
    let Stat::Return { list, .. } = &body[2] else {
        panic!("expected return");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.local_constant(vars[0].id).is_none());
    assert!(analysis.constant_expr(list[0].syntax_id()).is_none());
    assert!(!analysis.variable(vars[0].id).unwrap().is_constant());
}

#[test]
fn constant_analysis_folds_known_members_at_o2() {
    let root = parse_root("return game.answer");
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Return { list, .. } = &body[0] else {
        panic!("expected return");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 2,
        known_members: vec![super::super::options::KnownMember {
            library: String::from("game"),
            member: String::from("answer"),
            value: KnownMemberValue::Number { value: 3.5 },
        }],
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&ConstantValue::Number(3.5))
    );
}

#[test]
fn constant_analysis_folds_builtin_math_members_at_o2() {
    let root = parse_root("return math.pi, math.tau");
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Return { list, .. } = &body[0] else {
        panic!("expected return");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 2,
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&ConstantValue::Number(std::f64::consts::PI))
    );
    assert_eq!(
        analysis.constant_expr(list[1].syntax_id()),
        Some(&ConstantValue::Number(std::f64::consts::TAU))
    );
}

#[test]
fn constant_analysis_blocks_known_members_for_mutable_globals() {
    let root = parse_root("return game.answer");
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Return { list, .. } = &body[0] else {
        panic!("expected return");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 2,
        mutable_globals: vec![String::from("game")],
        known_members: vec![super::super::options::KnownMember {
            library: String::from("game"),
            member: String::from("answer"),
            value: KnownMemberValue::Number { value: 3.5 },
        }],
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    assert!(analysis.constant_expr(list[0].syntax_id()).is_none());
}

#[test]
fn constant_analysis_folds_builtin_calls_at_o2() {
    let root = parse_root(
        r#"
return
    string.char(49, 50, 0),
    string.sub("abcdef", 2, -2),
    math.abs(-3),
    bit32.band(7, 3),
    type(false),
    typeof(vector.create(1, 2, 3))
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Return { list, .. } = &body[0] else {
        panic!("expected return");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 2,
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&ConstantValue::String(String::from("12\0")))
    );
    assert_eq!(
        analysis.constant_expr(list[1].syntax_id()),
        Some(&ConstantValue::String(String::from("bcde")))
    );
    assert_eq!(
        analysis.constant_expr(list[2].syntax_id()),
        Some(&ConstantValue::Number(3.0))
    );
    assert_eq!(
        analysis.constant_expr(list[3].syntax_id()),
        Some(&ConstantValue::Number(3.0))
    );
    assert_eq!(
        analysis.constant_expr(list[4].syntax_id()),
        Some(&ConstantValue::String(String::from("boolean")))
    );
    assert!(analysis.constant_expr(list[5].syntax_id()).is_none());
}

#[test]
fn constant_analysis_folds_vector_constructors_and_components() {
    let root = parse_root(
        r#"
local v = vector.create(1, 2, 3)
return v.x
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { vars, .. } = &body[0] else {
        panic!("expected local");
    };
    let Stat::Return { list, .. } = &body[1] else {
        panic!("expected return");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 2,
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    assert_eq!(
        analysis.local_constant(vars[0].id),
        Some(&ConstantValue::Vector {
            bits: [
                1.0f32.to_bits(),
                2.0f32.to_bits(),
                3.0f32.to_bits(),
                0.0f32.to_bits()
            ]
        })
    );
    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&ConstantValue::Number(1.0))
    );
}

#[test]
fn constant_analysis_folds_vector_arithmetic() {
    let root = parse_root(
        r#"
local n = 2
local a, b = vector.create(1, 2, 3), vector.create(2, 4, 8)
return a + b, a - b, a * n, n * b, a / n, a / b, a * math.huge
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Return { list, .. } = &body[2] else {
        panic!("expected return");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 2,
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    let vector = |x: f32, y: f32, z: f32| ConstantValue::Vector {
        bits: [x.to_bits(), y.to_bits(), z.to_bits(), 0.0f32.to_bits()],
    };
    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&vector(3.0, 6.0, 11.0))
    );
    assert_eq!(
        analysis.constant_expr(list[1].syntax_id()),
        Some(&vector(-1.0, -2.0, -5.0))
    );
    assert_eq!(
        analysis.constant_expr(list[2].syntax_id()),
        Some(&vector(2.0, 4.0, 6.0))
    );
    assert_eq!(
        analysis.constant_expr(list[3].syntax_id()),
        Some(&vector(4.0, 8.0, 16.0))
    );
    assert_eq!(
        analysis.constant_expr(list[4].syntax_id()),
        Some(&vector(0.5, 1.0, 1.5))
    );
    assert!(analysis.constant_expr(list[5].syntax_id()).is_none());
    assert!(analysis.constant_expr(list[6].syntax_id()).is_none());
}

#[test]
fn constant_analysis_folds_four_wide_vector_arithmetic() {
    let root = parse_root(
        r#"
local n = 2
local a, b = vector.create(1, 2, 3, 4), vector.create(2, 4, 8, 1)
return a / b, n // b
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Return { list, .. } = &body[2] else {
        panic!("expected return");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 2,
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&ConstantValue::Vector {
            bits: [
                0.5f32.to_bits(),
                0.5f32.to_bits(),
                0.375f32.to_bits(),
                4.0f32.to_bits(),
            ]
        })
    );
    assert_eq!(
        analysis.constant_expr(list[1].syntax_id()),
        Some(&ConstantValue::Vector {
            bits: [
                1.0f32.to_bits(),
                0.0f32.to_bits(),
                0.0f32.to_bits(),
                2.0f32.to_bits(),
            ]
        })
    );
}

#[test]
fn constant_analysis_folds_safe_table_props() {
    let root = parse_root(
        r#"
local color = {red = 1, green = 2, blue = 3}
return color.red + color["green"] + color.blue
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { vars, .. } = &body[0] else {
        panic!("expected local");
    };
    let Stat::Return { list, .. } = &body[1] else {
        panic!("expected return");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert_eq!(
        analysis.table_prop(vars[0].id, "red"),
        Some(&ConstantValue::Number(1.0))
    );
    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&ConstantValue::Number(6.0))
    );
}

#[test]
fn constant_analysis_folds_short_circuiting_table_props() {
    let root = parse_root(
        r#"
local color = {red = 1, green = 2}
return color.red or color.green
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Return { list, .. } = &body[1] else {
        panic!("expected return");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert_eq!(
        analysis.constant_expr(list[0].syntax_id()),
        Some(&ConstantValue::Number(1.0))
    );
}

#[test]
fn constant_analysis_rejects_mutated_table_props() {
    let root = parse_root(
        r#"
local function id(x) return x end
local color = {red = 1}
id(color)
return color.red
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { vars, .. } = &body[1] else {
        panic!("expected table local");
    };
    let Stat::Return { list, .. } = &body[3] else {
        panic!("expected return");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.table_prop(vars[0].id, "red").is_none());
    assert!(analysis.constant_expr(list[0].syntax_id()).is_none());
}

#[test]
fn constant_analysis_rejects_table_props_after_table_key_escape() {
    let root = parse_root(
        r#"
local color = {red = 1}
u[color] = true
return color.red
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { vars, .. } = &body[0] else {
        panic!("expected table local");
    };
    let Stat::Return { list, .. } = &body[2] else {
        panic!("expected return");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.table_prop(vars[0].id, "red").is_none());
    assert!(analysis.constant_expr(list[0].syntax_id()).is_none());
}

#[test]
fn constant_analysis_rejects_ambiguous_table_prop_keys() {
    let root = parse_root(
        r#"
local empty = {[""] = 1}
local dup = {a = 1, ["a"] = 2}
local nul = {["a"] = 5, ["a\0"] = 2}
return empty[""], dup.a, nul.a - nul["a\0"]
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local {
        vars: empty_vars, ..
    } = &body[0]
    else {
        panic!("expected empty-key local");
    };
    let Stat::Local { vars: dup_vars, .. } = &body[1] else {
        panic!("expected duplicate-key local");
    };
    let Stat::Local { vars: nul_vars, .. } = &body[2] else {
        panic!("expected nul-key local");
    };
    let Stat::Return { list, .. } = &body[3] else {
        panic!("expected return");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.table_prop(empty_vars[0].id, "").is_none());
    assert!(analysis.table_prop(dup_vars[0].id, "a").is_none());
    assert_eq!(
        analysis.table_prop(nul_vars[0].id, "a"),
        Some(&ConstantValue::Number(5.0))
    );
    assert_eq!(
        analysis.constant_expr(list[2].syntax_id()),
        Some(&ConstantValue::Number(3.0))
    );
}

#[test]
fn constant_analysis_supports_always_terminates_queries() {
    let root = parse_root(
        r#"
if true then
    return 1
else
    side_effect()
end

if coin then
    return 1
else
    return 2
end
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert!(analysis.always_terminates(&body[0]));
    assert!(analysis.always_terminates(&body[1]));
    assert!(analysis.always_terminates(&root));
}

#[test]
fn constant_analysis_respects_optimization_level_zero() {
    let root = parse_root("local a = 1 + 2");
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { vars, values, .. } = &body[0] else {
        panic!("expected local");
    };
    let options = UpstreamCompilerOptions {
        optimization_level: 0,
        ..UpstreamCompilerOptions::default()
    };

    let (analysis, _) = collect_module_identities(&root, &options);

    assert!(analysis.constant_expr(values[0].syntax_id()).is_none());
    assert!(analysis.local_constant(vars[0].id).is_none());
    assert!(!analysis.variable(vars[0].id).unwrap().is_constant());
}

#[test]
fn table_shape_analysis_predicts_empty_table_fields() {
    let root = parse_root(
        r#"
local t = {}
t.a = 1
t.b = 2
t.a = 3
t[1] = 4
t[2] = 5
t[4] = 6
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { values, .. } = &body[0] else {
        panic!("expected local");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert_eq!(
        analysis.table_shape(values[0].syntax_id()),
        TableSizePrediction {
            hash_size: 2,
            array_size: 2
        }
    );
}

#[test]
fn table_shape_analysis_ignores_compound_assignments() {
    let root = parse_root("local t = {}\nt.foo += 5");
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { values, .. } = &body[0] else {
        panic!("expected local");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert_eq!(
        analysis.table_shape(values[0].syntax_id()),
        TableSizePrediction::default()
    );
}

#[test]
fn table_shape_analysis_tracks_setmetatable_and_numeric_loops() {
    let root = parse_root(
        r#"
local t = setmetatable({}, mt)
for i = 1, 4 do
    t[i] = i
end
"#,
    );
    let Stat::Block { body, .. } = &root else {
        panic!("expected block");
    };
    let Stat::Local { values, .. } = &body[0] else {
        panic!("expected local");
    };
    let Expr::Call { args, .. } = &values[0] else {
        panic!("expected setmetatable call");
    };

    let (analysis, _) = collect_module_identities(&root, &UpstreamCompilerOptions::default());

    assert_eq!(
        analysis.table_shape(args[0].syntax_id()),
        TableSizePrediction {
            hash_size: 0,
            array_size: 4
        }
    );
}

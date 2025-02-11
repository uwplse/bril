use std::collections::HashMap;

use inkwell::{
    basic_block::BasicBlock,
    builder::Builder,
    context::Context,
    intrinsics::Intrinsic,
    module::Module,
    types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType},
    values::{
        BasicMetadataValueEnum, BasicValue, BasicValueEnum, FloatValue, FunctionValue, IntValue,
        PointerValue,
    },
    AddressSpace, FloatPredicate, IntPredicate,
};

use bril_rs::{
    Argument, Code, ConstOps, EffectOps, Function, Instruction, Literal, Program, Type, ValueOps,
};

/// A helper function for performing operations over LLVM types
fn llvm_type_map<'ctx, A, F>(context: &'ctx Context, ty: &Type, mut fn_map: F) -> A
where
    F: for<'a> FnMut(BasicTypeEnum<'ctx>) -> A,
{
    match ty {
        Type::Int => fn_map(context.i64_type().into()),
        Type::Bool => fn_map(context.bool_type().into()),
        Type::Float => fn_map(context.f64_type().into()),
        Type::Pointer(_) => fn_map(context.ptr_type(AddressSpace::default()).into()),
    }
}

fn unwrap_bril_ptrtype(ty: &Type) -> &Type {
    match ty {
        Type::Pointer(ty) => ty,
        _ => unreachable!(),
    }
}

/// Converts a Bril function signature into an LLVM function type
fn build_functiontype<'a>(
    context: &'a Context,
    args: &[&Type],
    return_ty: &Option<Type>,
) -> FunctionType<'a> {
    let param_types: Vec<BasicMetadataTypeEnum> = args
        .iter()
        .map(|t| llvm_type_map(context, t, Into::into))
        .collect();
    #[allow(clippy::option_if_let_else)] // I think this is more readable
    match return_ty {
        None => context.void_type().fn_type(&param_types, false),
        Some(t) => llvm_type_map(context, t, |t| t.fn_type(&param_types, false)),
    }
}

fn build_load<'a>(
    context: &'a Context,
    builder: &'a Builder,
    ptr: &WrappedPointer<'a>,
    name: &str,
) -> BasicValueEnum<'a> {
    llvm_type_map(context, &ptr.ty, |pointee_ty| {
        builder.build_load(pointee_ty, ptr.ptr, name).unwrap()
    })
}

// Type information is needed for cases like Bool which is modelled as an int and is as far as I can tell indistinguishable.
#[derive(Debug, Clone)]
struct WrappedPointer<'a> {
    ty: Type,
    ptr: PointerValue<'a>,
}

impl<'a> WrappedPointer<'a> {
    fn new(builder: &'a Builder, context: &'a Context, name: &str, ty: &Type) -> Self {
        Self {
            ty: ty.clone(),
            ptr: llvm_type_map(context, ty, |ty| builder.build_alloca(ty, name).unwrap()),
        }
    }
}

#[derive(Default)]
struct Heap<'a, 'b> {
    // Map variable names in Bril to their type and location on the stack.
    map: HashMap<&'b String, WrappedPointer<'a>>,
}

impl<'a, 'b> Heap<'a, 'b> {
    fn new() -> Self {
        Self::default()
    }

    fn add(
        &mut self,
        builder: &'a Builder,
        context: &'a Context,
        name: &'b String,
        ty: &Type,
    ) -> WrappedPointer<'a> {
        let result = self
            .map
            .entry(name)
            .or_insert_with(|| WrappedPointer::new(builder, context, name, ty))
            .clone();
        if result.ty != *ty {
            println!(
                "`{}` had type `{}` but is now being assigned type `{}`",
                name, result.ty, ty
            );
            unimplemented!("brillvm does not currently support variables within a function having different types. Implementing this might require a control flow analysis? Feel free to try and implement this.")
        }
        result
    }

    fn get(&self, name: &String) -> WrappedPointer<'a> {
        self.map.get(name).unwrap().clone()
    }
}

#[derive(Default)]
struct Fresh {
    count: u64,
}

impl Fresh {
    fn new() -> Self {
        Self::default()
    }

    fn fresh_label(&mut self) -> String {
        let l = format!("label{}", self.count);
        self.count += 1;
        l
    }

    fn fresh_var(&mut self) -> String {
        let v = format!("var{}", self.count);
        self.count += 1;
        v
    }
}

// This handles the builder boilerplate of creating loads for the arguments of a function and the the corresponding store of the result.
fn build_op<'a, 'b>(
    context: &'a Context,
    builder: &'a Builder,
    heap: &Heap<'a, 'b>,
    fresh: &mut Fresh,
    op: impl Fn(Vec<BasicValueEnum<'a>>) -> BasicValueEnum<'a>,
    args: &'b [String],
    dest: &'b String,
) {
    builder
        .build_store(
            heap.get(dest).ptr,
            op(args
                .iter()
                .map(|n| build_load(context, builder, &heap.get(n), &fresh.fresh_var()))
                .collect()),
        )
        .unwrap();
}

// Like `build_op` but where there is no return value
fn build_effect_op<'a, 'b>(
    context: &'a Context,
    builder: &'a Builder,
    heap: &Heap<'a, 'b>,
    fresh: &mut Fresh,
    op: impl Fn(Vec<BasicValueEnum<'a>>),
    args: &'b [String],
) {
    op(args
        .iter()
        .map(|n| build_load(context, builder, &heap.get(n), &fresh.fresh_var()))
        .collect());
}

// Handles the map of labels to LLVM Basicblocks and creates a new one when it doesn't exist
fn block_map_get<'a>(
    context: &'a Context,
    llvm_func: FunctionValue<'a>,
    block_map: &mut HashMap<String, BasicBlock<'a>>,
    name: &str,
) -> BasicBlock<'a> {
    *block_map
        .entry(name.to_owned())
        .or_insert_with(|| context.append_basic_block(llvm_func, name))
}

// The workhorse of converting a Bril Instruction to an LLVM Instruction
#[allow(clippy::too_many_arguments)]
fn build_instruction<'a, 'b>(
    i: &'b Instruction,
    context: &'a Context,
    module: &'a Module,
    builder: &'a Builder,
    heap: &Heap<'a, 'b>,
    block_map: &mut HashMap<String, BasicBlock<'a>>,
    llvm_func: FunctionValue<'a>,
    fresh: &mut Fresh,
) {
    match i {
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Abs,
            op_type: _,
        } => {
            let abs_intrinsic = Intrinsic::find("llvm.abs.i64").unwrap();
            let abs_fn = abs_intrinsic
                .get_declaration(&module, &[BasicTypeEnum::IntType(context.i64_type())])
                .unwrap();

            let ret_name = fresh.fresh_var();

            // second arg to llvm.abs is a boolean flag indicating
            // whether the result value of the ‘llvm.abs’ intrinsic is a poison value if the
            // first arg is INT_MIN
            // https://llvm.org/docs/LangRef.html#llvm-abs-intrinsic
            let fals = BasicValueEnum::IntValue(context.bool_type().const_int(0, false));

            let mut args: Vec<BasicMetadataValueEnum> = args
                .iter()
                .map(|n| build_load(context, builder, &heap.get(n), &fresh.fresh_var()).into())
                .collect();

            args.push(fals.into());

            let op = builder
                .build_call(abs_fn, &args, &ret_name)
                .unwrap()
                .try_as_basic_value()
                .left()
                .unwrap();

            builder.build_store(heap.get(dest).ptr, op).unwrap();
        }
        // Special case where Bril casts integers to floats
        Instruction::Constant {
            dest,
            op: ConstOps::Const,
            const_type: Type::Float,
            value: Literal::Int(i),
        } => {
            #[allow(clippy::cast_precision_loss)]
            builder
                .build_store(
                    heap.get(dest).ptr,
                    context.f64_type().const_float(*i as f64),
                )
                .unwrap();
        }
        Instruction::Constant {
            dest,
            op: ConstOps::Const,
            const_type: _,
            value: Literal::Int(i),
        } => {
            #[allow(clippy::cast_sign_loss)]
            builder
                .build_store(
                    heap.get(dest).ptr,
                    context.i64_type().const_int(*i as u64, true),
                )
                .unwrap();
        }
        Instruction::Constant {
            dest,
            op: ConstOps::Const,
            const_type: _,
            value: Literal::Bool(b),
        } => {
            builder
                .build_store(
                    heap.get(dest).ptr,
                    context.bool_type().const_int((*b).into(), false),
                )
                .unwrap();
        }
        Instruction::Constant {
            dest,
            op: ConstOps::Const,
            const_type: _,
            value: Literal::Float(f),
        } => {
            builder
                .build_store(heap.get(dest).ptr, context.f64_type().const_float(*f))
                .unwrap();
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Bitand,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_and::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Add,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_add::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Sub,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_sub::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Mul,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_mul::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Div,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_signed_div::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Eq,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_compare::<IntValue>(
                            IntPredicate::EQ,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Lt,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_compare::<IntValue>(
                            IntPredicate::SLT,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Gt,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_compare::<IntValue>(
                            IntPredicate::SGT,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Le,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_compare::<IntValue>(
                            IntPredicate::SLE,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Ge,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_compare::<IntValue>(
                            IntPredicate::SGE,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Neg,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_int_neg::<IntValue>(v[0].try_into().unwrap(), &ret_name)
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Not,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_not::<IntValue>(v[0].try_into().unwrap(), &ret_name)
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::And,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_and::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Or,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_or::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs,
            labels: _,
            op: ValueOps::Call,
            op_type: _,
        } => {
            let func_name = if funcs[0] == "main" {
                "_main"
            } else {
                &funcs[0]
            };
            let function = module.get_function(func_name).unwrap();
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_call(
                            function,
                            v.iter()
                                .map(|val| (*val).into())
                                .collect::<Vec<_>>()
                                .as_slice(),
                            &ret_name,
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .left()
                        .unwrap()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Id,
            op_type: _,
        } => build_op(context, builder, heap, fresh, |v| v[0], args, dest),

        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Select,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_select::<BasicValueEnum, IntValue>(
                            v[0].try_into().unwrap(),
                            v[1],
                            v[2],
                            &ret_name,
                        )
                        .unwrap()
                },
                args,
                dest,
            );
        }

        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Smax,
            op_type: _,
        } => {
            let smax_intrinsic = Intrinsic::find("llvm.smax.i64").unwrap();
            let smax_fn = smax_intrinsic
                .get_declaration(&module, &[BasicTypeEnum::IntType(context.i64_type())])
                .unwrap();

            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_call(
                            smax_fn,
                            v.iter()
                                .map(|val| (*val).into())
                                .collect::<Vec<_>>()
                                .as_slice(),
                            &ret_name,
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .left()
                        .unwrap()
                },
                args,
                dest,
            );
        }

        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Smin,
            op_type: _,
        } => {
            let smin_intrinsic = Intrinsic::find("llvm.smin.i64").unwrap();
            let smin_fn = smin_intrinsic
                .get_declaration(&module, &[BasicTypeEnum::IntType(context.i64_type())])
                .unwrap();

            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_call(
                            smin_fn,
                            v.iter()
                                .map(|val| (*val).into())
                                .collect::<Vec<_>>()
                                .as_slice(),
                            &ret_name,
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .left()
                        .unwrap()
                },
                args,
                dest,
            );
        }

        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Shl,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_left_shift::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }

        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Shr,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_right_shift::<IntValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            false, // sign extend
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }

        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fadd,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_add::<FloatValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fsub,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_sub::<FloatValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fmul,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_mul::<FloatValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fdiv,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_div::<FloatValue>(
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Feq,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_compare::<FloatValue>(
                            FloatPredicate::OEQ,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Flt,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_compare::<FloatValue>(
                            FloatPredicate::OLT,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fgt,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_compare::<FloatValue>(
                            FloatPredicate::OGT,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fle,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_compare::<FloatValue>(
                            FloatPredicate::OLE,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fge,
            op_type: _,
        } => {
            let ret_name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_float_compare::<FloatValue>(
                            FloatPredicate::OGE,
                            v[0].try_into().unwrap(),
                            v[1].try_into().unwrap(),
                            &ret_name,
                        )
                        .unwrap()
                        .into()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fmax,
            op_type: _,
        } => {
            let cmp_name = fresh.fresh_var();
            let name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_select(
                            builder
                                .build_float_compare::<FloatValue>(
                                    FloatPredicate::OGT,
                                    v[0].try_into().unwrap(),
                                    v[1].try_into().unwrap(),
                                    &cmp_name,
                                )
                                .unwrap(),
                            v[0],
                            v[1],
                            &name,
                        )
                        .unwrap()
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Fmin,
            op_type: _,
        } => {
            let cmp_name = fresh.fresh_var();
            let name = fresh.fresh_var();
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_select(
                            builder
                                .build_float_compare::<FloatValue>(
                                    FloatPredicate::OLT,
                                    v[0].try_into().unwrap(),
                                    v[1].try_into().unwrap(),
                                    &cmp_name,
                                )
                                .unwrap(),
                            v[0],
                            v[1],
                            &name,
                        )
                        .unwrap()
                },
                args,
                dest,
            );
        }

        Instruction::Effect {
            args,
            funcs: _,
            labels: _,
            op: EffectOps::Return,
        } => {
            if args.is_empty() {
                builder.build_return(None).unwrap();
            } else {
                builder
                    .build_return(Some(&build_load(
                        context,
                        builder,
                        &heap.get(&args[0]),
                        &fresh.fresh_var(),
                    )))
                    .unwrap();
            }
        }
        Instruction::Effect {
            args,
            funcs,
            labels: _,
            op: EffectOps::Call,
        } => {
            let func_name = if funcs[0] == "main" {
                "_main"
            } else {
                &funcs[0]
            };
            let function = module.get_function(func_name).unwrap();
            let ret_name = fresh.fresh_var();
            build_effect_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_call(
                            function,
                            v.iter()
                                .map(|val| (*val).into())
                                .collect::<Vec<_>>()
                                .as_slice(),
                            &ret_name,
                        )
                        .unwrap();
                },
                args,
            );
        }
        Instruction::Effect {
            args: _,
            funcs: _,
            labels: _,
            op: EffectOps::Nop,
        } => {}
        Instruction::Effect {
            args,
            funcs: _,
            labels: _,
            op: EffectOps::Print,
        } => {
            let print_int = module.get_function("_bril_print_int").unwrap();
            let print_bool = module.get_function("_bril_print_bool").unwrap();
            let print_float = module.get_function("_bril_print_float").unwrap();
            let print_sep = module.get_function("_bril_print_sep").unwrap();
            let print_end = module.get_function("_bril_print_end").unwrap();
            /*            let ret_name = fresh.fresh_var(); */
            let len = args.len();

            args.iter().enumerate().for_each(|(i, a)| {
                let wrapped_ptr = heap.get(a);
                let v = build_load(context, builder, &wrapped_ptr, &fresh.fresh_var());
                match wrapped_ptr.ty {
                    Type::Int => {
                        builder
                            .build_call(print_int, &[v.into()], "print_int")
                            .unwrap();
                    }
                    Type::Bool => {
                        builder
                            .build_call(
                                print_bool,
                                &[builder
                                    .build_int_cast::<IntValue>(
                                        v.try_into().unwrap(),
                                        context.bool_type(),
                                        "bool_cast",
                                    )
                                    .unwrap()
                                    .into()],
                                "print_bool",
                            )
                            .unwrap();
                    }
                    Type::Float => {
                        builder
                            .build_call(print_float, &[v.into()], "print_float")
                            .unwrap();
                    }
                    Type::Pointer(_) => {
                        unreachable!()
                    }
                };
                if i < len - 1 {
                    builder.build_call(print_sep, &[], "print_sep").unwrap();
                }
            });
            builder.build_call(print_end, &[], "print_end").unwrap();
        }
        Instruction::Effect {
            args: _,
            funcs: _,
            labels,
            op: EffectOps::Jump,
        } => {
            builder
                .build_unconditional_branch(block_map_get(
                    context, llvm_func, block_map, &labels[0],
                ))
                .unwrap();
        }
        Instruction::Effect {
            args,
            funcs: _,
            labels,
            op: EffectOps::Branch,
        } => {
            let then_block = block_map_get(context, llvm_func, block_map, &labels[0]);
            let else_block = block_map_get(context, llvm_func, block_map, &labels[1]);
            build_effect_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder
                        .build_conditional_branch(v[0].try_into().unwrap(), then_block, else_block)
                        .unwrap();
                },
                args,
            );
        }
        Instruction::Value {
            args: __args,
            dest: _dest,
            funcs: _,
            labels: _,
            op: ValueOps::Phi,
            op_type: _op_type,
        } => {
            panic!("Phi nodes should be handled by build_phi");
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Alloc,
            op_type,
        } => {
            let alloc_name = fresh.fresh_var();
            let ty = unwrap_bril_ptrtype(op_type);
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    llvm_type_map(context, ty, |ty| {
                        builder
                            .build_array_malloc(ty, v[0].try_into().unwrap(), &alloc_name)
                            .unwrap()
                            .into()
                    })
                },
                args,
                dest,
            );
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Load,
            op_type,
        } => {
            let name = fresh.fresh_var();
            llvm_type_map(context, op_type, |pointee_ty| {
                build_op(
                    context,
                    builder,
                    heap,
                    fresh,
                    |v| {
                        builder
                            .build_load(pointee_ty, v[0].try_into().unwrap(), &name)
                            .unwrap()
                    },
                    args,
                    dest,
                );
            });
        }
        Instruction::Value {
            args,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::PtrAdd,
            op_type,
        } => {
            let name = fresh.fresh_var();
            let op_type = unwrap_bril_ptrtype(op_type);
            build_op(
                context,
                builder,
                heap,
                fresh,
                |v| unsafe {
                    llvm_type_map(context, op_type, |pointee_ty| {
                        builder
                            .build_gep(
                                pointee_ty,
                                v[0].try_into().unwrap(),
                                &[v[1].try_into().unwrap()],
                                &name,
                            )
                            .unwrap()
                            .into()
                    })
                },
                args,
                dest,
            );
        }
        Instruction::Effect {
            args,
            funcs: _,
            labels: _,
            op: EffectOps::Store,
        } => {
            build_effect_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder.build_store(v[0].try_into().unwrap(), v[1]).unwrap();
                },
                args,
            );
        }
        Instruction::Effect {
            args,
            funcs: _,
            labels: _,
            op: EffectOps::Free,
        } => {
            build_effect_op(
                context,
                builder,
                heap,
                fresh,
                |v| {
                    builder.build_free(v[0].try_into().unwrap()).unwrap();
                },
                args,
            );
        }
    }
}

// Check for instructions that end a block
const fn is_terminating_instr(i: &Option<Instruction>) -> bool {
    matches!(
        i,
        Some(Instruction::Effect {
            args: _,
            funcs: _,
            labels: _,
            op: EffectOps::Branch | EffectOps::Jump | EffectOps::Return,
        })
    )
}

/// Given a Bril program, create an LLVM module from it
/// The `runtime_module` is the module containing the runtime library
/// # Panics
/// Panics if the program is invalid
#[must_use]
pub fn create_module_from_program<'a>(
    context: &'a Context,
    Program { functions, .. }: &Program,
    runtime_module: Module<'a>,
    add_timing: bool,
) -> Module<'a> {
    let builder = context.create_builder();

    // "Global" counter for creating labels/temp variable names
    let mut fresh = Fresh::new();

    // Add all functions to the module, initialize all variables in the heap, and setup for the second phase
    #[allow(clippy::needless_collect)]
    let funcs: Vec<_> = functions
        .iter()
        .map(
            |Function {
                 args,
                 instrs,
                 name,
                 return_type,
             }| {
                // Setup function in module
                let ty = build_functiontype(
                    context,
                    &args
                        .iter()
                        .map(|Argument { arg_type, .. }| arg_type)
                        .collect::<Vec<_>>(),
                    return_type,
                );

                let func_name = if name == "main" { "_main" } else { name };

                let llvm_func = runtime_module.add_function(func_name, ty, None);
                args.iter().zip(llvm_func.get_param_iter()).for_each(
                    |(Argument { name, .. }, bve)| match bve {
                        inkwell::values::BasicValueEnum::IntValue(i) => i.set_name(name),
                        inkwell::values::BasicValueEnum::FloatValue(f) => f.set_name(name),
                        inkwell::values::BasicValueEnum::PointerValue(p) => p.set_name(name),
                        inkwell::values::BasicValueEnum::ArrayValue(_)
                        | inkwell::values::BasicValueEnum::StructValue(_)
                        | inkwell::values::BasicValueEnum::VectorValue(_) => unreachable!(),
                    },
                );

                // For each function, we also need to push all variables onto the stack
                let mut heap = Heap::new();
                let block = context.append_basic_block(llvm_func, &fresh.fresh_label());
                builder.position_at_end(block);

                llvm_func.get_param_iter().enumerate().for_each(|(i, arg)| {
                    let Argument { name, arg_type } = &args[i];
                    let ptr = heap.add(&builder, context, name, arg_type).ptr;
                    builder.build_store(ptr, arg).unwrap();
                });

                instrs.iter().for_each(|i| match i {
                    Code::Label { .. } | Code::Instruction(Instruction::Effect { .. }) => {}
                    Code::Instruction(Instruction::Constant {
                        dest, const_type, ..
                    }) => {
                        heap.add(&builder, context, dest, const_type);
                    }
                    Code::Instruction(Instruction::Value { dest, op_type, .. }) => {
                        heap.add(&builder, context, dest, op_type);
                    }
                });

                (llvm_func, instrs, block, heap, return_type)
            },
        )
        .collect(); // Important to collect, can't be done lazily because we need all functions to be loaded in before a call instruction of a function is processed.

    // Now actually build each function
    let mut added_timing = false;
    let mut ticks_start_ref = None;
    funcs
        .into_iter()
        .for_each(|(llvm_func, instrs, mut block, heap, return_type)| {
            let mut last_instr = None;

            // Maps labels to llvm blocks for jumps
            let mut block_map = HashMap::new();


            // If there are actually instructions, proceed
            if !instrs.is_empty() {
                builder.position_at_end(block);

                // When we are in main, start measuring time
                if add_timing && llvm_func.get_name().to_str().unwrap() == "_main" {
                    let ticks_start_name = fresh.fresh_var();
                    // get_ticks_start is used on x86 and get_ticks is used on arm
                    #[cfg(target_arch = "x86_64")]
                    let get_ticks_start = "_bril_get_ticks_start";
                    #[cfg(target_arch = "aarch64")]
                    let get_ticks_start = "_bril_get_ticks";
                    let ticks_start = builder
                        .build_call(
                            runtime_module.get_function(get_ticks_start).unwrap(),
                            &[],
                            &ticks_start_name,
                        )
                        .unwrap()
                        .try_as_basic_value()
                        .unwrap_left();
                    ticks_start_ref = Some(ticks_start);
                    // TODO I would like to inline get_ticks_start for less overhead
                    // however, this results in segfaults for some reason
                    /*let func = runtime_module.get_function(get_ticks_start).unwrap();
                    func.remove_enum_attribute(AttributeLoc::Function, 28);
                    func.add_attribute(AttributeLoc::Function, context.create_enum_attribute(3, 1));*/
                }

                let mut index = 0;
                while index < instrs.len() {
                    // for main, we expect the last instruction to be a print
                    if add_timing && llvm_func.get_name().to_str().unwrap() == "_main"
                        && matches!(
                            instrs[index],
                            Code::Instruction(Instruction::Effect {
                                op: EffectOps::Print,
                                ..
                            })
                        )
                    {
                        // either this is the last instruction or the next one is a return
                        assert!(
                            index == instrs.len() - 1
                                || matches!(
                                    instrs[index + 1],
                                    Code::Instruction(Instruction::Effect {
                                        op: EffectOps::Return,
                                        ..
                                    })
                                )
                        );

                        // measure cycles and print
                        let ticks_end_name = fresh.fresh_var();
                        #[cfg(target_arch = "x86_64")]
                        let get_ticks_end = "_bril_get_ticks_end";
                        #[cfg(target_arch = "aarch64")]
                        let get_ticks_end = "_bril_get_ticks";
                        // TODO I would like to inline get_ticks_start for less overhead
                        // however, this results in segfaults for some reason
                        /*let func = runtime_module.get_function(get_ticks_end).unwrap();
                        // always inline get_ticks_end
                        func.remove_enum_attribute(AttributeLoc::Function, 28);
                        func.add_attribute(
                            AttributeLoc::Function,
                            context.create_enum_attribute(3, 1),
                        );*/

                        let ticks_end = builder
                            .build_call(
                                runtime_module.get_function(get_ticks_end).unwrap(),
                                &[],
                                &ticks_end_name,
                            )
                            .unwrap()
                            .try_as_basic_value()
                            .unwrap_left();

                        // print out the different between the ticks
                        let ticks_diff = fresh.fresh_var();
                        let diff_val = builder
                            .build_int_sub::<IntValue>(
                                ticks_end.try_into().unwrap(),
                                ticks_start_ref.unwrap().try_into().unwrap(),
                                &ticks_diff,
                            )
                            .unwrap();

                        // use bril_print_unsiged_int to print out the difference
                        let print_ticks = runtime_module
                            .get_function("_bril_eprintln_unsigned_int")
                            .unwrap();
                        builder
                            .build_call(print_ticks, &[diff_val.into()], "print_ticks")
                            .unwrap();
                        added_timing = true;
                    }

                    if is_terminating_instr(&last_instr)
                        && matches!(instrs[index], Code::Instruction { .. })
                    {
                        index += 1;
                        continue;
                    }

                    let mut phi_index = index;
                    let mut phi_ptrs = vec![];
                    while phi_index < instrs.len() && is_phi(&instrs[phi_index]) {
                        match &instrs[phi_index] {
                            Code::Instruction(instr) => {
                                phi_ptrs.push((
                                    instr.clone(),
                                    build_phi(
                                        instr,
                                        context,
                                        &runtime_module,
                                        &builder,
                                        &heap,
                                        &mut block_map,
                                        llvm_func,
                                        &mut fresh,
                                    ),
                                ));
                                last_instr = Some(instr.clone());
                            }
                            Code::Label { .. } => unreachable!(),
                        }
                        phi_index += 1;
                    }

                    for (instr, phi) in phi_ptrs {
                        finish_phi(
                            &instr,
                            context,
                            &runtime_module,
                            &builder,
                            &heap,
                            &mut fresh,
                            phi,
                        );
                    }
                    if phi_index > index {
                        index = phi_index;
                        continue;
                    }

                    match &instrs[index] {
                        bril_rs::Code::Label { label, .. } => {
                            let new_block =
                                block_map_get(context, llvm_func, &mut block_map, label);

                            // Check if wee need to insert a jump since all llvm blocks must be terminated
                            if !is_terminating_instr(&last_instr) {
                                builder
                                    .build_unconditional_branch(block_map_get(
                                        context,
                                        llvm_func,
                                        &mut block_map,
                                        label,
                                    ))
                                    .unwrap();
                            }

                            // Start a new block
                            block = new_block;
                            builder.position_at_end(block);
                            last_instr = None;
                        }
                        bril_rs::Code::Instruction(i) => {
                            build_instruction(
                                i,
                                context,
                                &runtime_module,
                                &builder,
                                &heap,
                                &mut block_map,
                                llvm_func,
                                &mut fresh,
                            );
                            last_instr = Some(i.clone());
                        }
                    }
                    index += 1;
                }
            }

            // Make sure every function is terminated with a return if not already
            if !is_terminating_instr(&last_instr) {
                if return_type.is_none() {
                    builder.build_return(None).unwrap();
                } else {
                    // This block did not have a terminating instruction
                    // Returning void is ill-typed for this function
                    // This code should be unreachable in well-formed Bril
                    // Let's just arbitrarily jump to avoid needing to
                    // instantiate a valid return value.
                    assert!(!block_map.is_empty());
                    builder
                        .build_unconditional_branch(*block_map.values().next().unwrap())
                        .unwrap();
                }
            }
        });

    if add_timing {
        assert!(added_timing);
    }

    // Add new main function to act as a entry point to the function.
    // Sets up arguments for a _main call
    // and always returns zero
    let entry_func_type = context.i32_type().fn_type(
        &[
            context.i32_type().into(),
            context.ptr_type(AddressSpace::default()).into(),
        ],
        false,
    );
    let entry_func = runtime_module.add_function("main", entry_func_type, None);
    entry_func.get_nth_param(0).unwrap().set_name("argc");
    entry_func.get_nth_param(1).unwrap().set_name("argv");

    let entry_block = context.append_basic_block(entry_func, &fresh.fresh_label());
    builder.position_at_end(entry_block);

    let mut heap = Heap::new();

    if let Some(function) = runtime_module.get_function("_main") {
        let Function { args, .. } = functions
            .iter()
            .find(|Function { name, .. }| name == "main")
            .unwrap();

        let argv = entry_func.get_nth_param(1).unwrap().into_pointer_value();

        let parse_int = runtime_module.get_function("_bril_parse_int").unwrap();
        let parse_bool = runtime_module.get_function("_bril_parse_bool").unwrap();
        let parse_float = runtime_module.get_function("_bril_parse_float").unwrap();

        function.get_param_iter().enumerate().for_each(|(i, _)| {
            let Argument { name, arg_type } = &args[i];
            let ptr = heap.add(&builder, context, name, arg_type).ptr;
            let arg_str = builder
                .build_load(
                    context.ptr_type(AddressSpace::default()),
                    unsafe {
                        builder.build_in_bounds_gep(
                            context.ptr_type(AddressSpace::default()),
                            argv,
                            &[context.i64_type().const_int((i + 1) as u64, true)],
                            "calculate offset",
                        )
                    }
                    .unwrap(),
                    "load arg",
                )
                .unwrap();
            let arg = match arg_type {
                Type::Int => builder
                    .build_call(parse_int, &[arg_str.into()], "parse_int")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_left(),
                Type::Bool => builder
                    .build_call(parse_bool, &[arg_str.into()], "parse_bool")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_left(),
                Type::Float => builder
                    .build_call(parse_float, &[arg_str.into()], "parse_float")
                    .unwrap()
                    .try_as_basic_value()
                    .unwrap_left(),
                Type::Pointer(_) => unreachable!(),
            };
            builder.build_store(ptr, arg).unwrap();
        });

        build_effect_op(
            context,
            &builder,
            &heap,
            &mut fresh,
            |v| {
                builder
                    .build_call(
                        function,
                        v.iter()
                            .map(|val| (*val).into())
                            .collect::<Vec<_>>()
                            .as_slice(),
                        "call main",
                    )
                    .unwrap();
            },
            &args
                .iter()
                .map(|Argument { name, .. }| name.clone())
                .collect::<Vec<String>>(),
        );
    }
    builder
        .build_return(Some(&context.i32_type().const_int(0, true)))
        .unwrap();

    // Return the module
    runtime_module
}

pub(crate) const fn is_phi(i: &Code) -> bool {
    matches!(
        i,
        Code::Instruction(Instruction::Value {
            op: ValueOps::Phi,
            ..
        })
    )
}

// The workhorse of converting a Bril Instruction to an LLVM Instruction
#[allow(clippy::too_many_arguments)]
fn build_phi<'a, 'b>(
    i: &'b Instruction,
    context: &'a Context,
    _module: &'a Module,
    builder: &'a Builder,
    heap: &Heap<'a, 'b>,
    block_map: &mut HashMap<String, BasicBlock<'a>>,
    llvm_func: FunctionValue<'a>,
    fresh: &mut Fresh,
) -> PointerValue<'a> {
    match i {
        Instruction::Value {
            args,
            dest: _,
            funcs: _,
            labels,
            op: ValueOps::Phi,
            op_type: _,
        } => {
            let name = fresh.fresh_var();
            let blocks = labels
                .iter()
                .map(|l| block_map_get(context, llvm_func, block_map, l))
                .collect::<Vec<_>>();

            let phi = builder
                .build_phi(context.ptr_type(AddressSpace::default()), &name)
                .unwrap();

            let pointers = args.iter().map(|a| heap.get(a).ptr).collect::<Vec<_>>();

            // The phi node is a little non-standard since we can't load in values from the stack before the phi instruction. Instead, the phi instruction will be over stack locations which will then be loaded into the corresponding output location.
            phi.add_incoming(
                pointers
                    .iter()
                    .zip(blocks.iter())
                    .map(|(val, block)| (val as &dyn BasicValue, *block))
                    .collect::<Vec<_>>()
                    .as_slice(),
            );

            phi.as_basic_value().into_pointer_value()
        }
        _ => unreachable!(),
    }
}

/// finish the phi by loading in the value
#[allow(clippy::too_many_arguments)]
fn finish_phi<'a, 'b>(
    i: &'b Instruction,
    context: &'a Context,
    _module: &'a Module,
    builder: &'a Builder,
    heap: &Heap<'a, 'b>,
    fresh: &mut Fresh,
    ptr: PointerValue<'a>,
) {
    match i {
        Instruction::Value {
            args: _,
            dest,
            funcs: _,
            labels: _,
            op: ValueOps::Phi,
            op_type,
        } => {
            builder
                .build_store(
                    heap.get(dest).ptr,
                    build_load(
                        context,
                        builder,
                        &WrappedPointer {
                            ty: op_type.clone(),
                            ptr,
                        },
                        &fresh.fresh_var(),
                    ),
                )
                .unwrap();
        }
        _ => unreachable!(),
    }
}

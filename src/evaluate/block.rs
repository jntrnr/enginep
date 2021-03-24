use crate::{empty_value_iterator, evaluate::expr::run_expression_block};
use crate::{evaluate::evaluation_context::EvaluationContext, ValueIterator};
use crate::{evaluate::internal::run_internal_command, Scope};
use nu_errors::ShellError;
use nu_parser::ParserScope;
use nu_protocol::hir::{
    Block, Call, ClassifiedCommand, Expression, Pipeline, SpannedExpression, Synthetic,
};
use nu_protocol::{ReturnSuccess, UntaggedValue, Value};
use nu_source::{Span, Tag};
use std::sync::atomic::Ordering;

pub fn run_block(
    block: &Block,
    ctx: &EvaluationContext,
    scope: &Scope,
    mut input: ValueIterator,
) -> Result<ValueIterator, ShellError> {
    let mut output: Result<ValueIterator, ShellError> = Ok(empty_value_iterator());
    for (_, definition) in block.definitions.iter() {
        ctx.scope.add_definition(definition.clone());
    }

    for group in &block.block {
        match output {
            Ok(inp) if inp.is_empty() => {}
            Ok(inp) => {
                // Run autoview on the values we've seen so far
                // We may want to make this configurable for other kinds of hosting
                if let Some(autoview) = scope.get_command("autoview") {
                    let mut output_stream = match ctx.run_command(
                        autoview,
                        Tag::unknown(),
                        Call::new(
                            Box::new(SpannedExpression::new(
                                Expression::Synthetic(Synthetic::String("autoview".into())),
                                Span::unknown(),
                            )),
                            Span::unknown(),
                        ),
                        inp,
                        scope,
                    ) {
                        Ok(x) => x,
                        Err(e) => {
                            return Err(e);
                        }
                    };
                    match output_stream.next() {
                        Ok(Some(ReturnSuccess::Value(Value {
                            value: UntaggedValue::Error(e),
                            ..
                        }))) => {
                            return Err(e);
                        }
                        Ok(Some(_item)) => {
                            if let Some(err) = ctx.get_errors().get(0) {
                                ctx.clear_errors();
                                return Err(err.clone());
                            }
                            if ctx.ctrl_c.load(Ordering::SeqCst) {
                                return Ok(InputStream::empty());
                            }
                        }
                        Ok(None) => {
                            if let Some(err) = ctx.get_errors().get(0) {
                                ctx.clear_errors();
                                return Err(err.clone());
                            }
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }
            }
            Err(e) => {
                return Err(e);
            }
        }
        output = Ok(empty_value_iterator());
        for pipeline in &group.pipelines {
            match output {
                Ok(inp) if inp.is_empty() => {}
                Ok(inp) => {
                    let mut output_stream = inp.to_output_stream();

                    match output_stream.try_next() {
                        Ok(Some(ReturnSuccess::Value(Value {
                            value: UntaggedValue::Error(e),
                            ..
                        }))) => {
                            return Err(e);
                        }
                        Ok(Some(_item)) => {
                            if let Some(err) = ctx.get_errors().get(0) {
                                ctx.clear_errors();
                                return Err(err.clone());
                            }
                            if ctx.ctrl_c.load(Ordering::SeqCst) {
                                // This early return doesn't return the result
                                // we have so far, but breaking out of this loop
                                // causes lifetime issues. A future contribution
                                // could attempt to return the current output.
                                // https://github.com/nushell/nushell/pull/2830#discussion_r550319687
                                return Ok(empty_value_iterator());
                            }
                        }
                        Ok(None) => {
                            if let Some(err) = ctx.get_errors().get(0) {
                                ctx.clear_errors();
                                return Err(err.clone());
                            }
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }
                Err(e) => {
                    return Err(e);
                }
            }
            output = run_pipeline(pipeline, ctx, scope, input);

            input = empty_value_iterator();
        }
    }

    output
}

fn run_pipeline(
    commands: &Pipeline,
    ctx: &EvaluationContext,
    scope: &Scope,
    mut input: ValueIterator,
) -> Result<ValueIterator, ShellError> {
    for item in commands.list.clone() {
        input = match item {
            ClassifiedCommand::Dynamic(call) => {
                let mut args = vec![];
                if let Some(positional) = call.positional {
                    for pos in &positional {
                        let result = run_expression_block(pos, ctx)?.into_vec();
                        args.push(result);
                    }
                }

                match &call.head.expr {
                    Expression::Block(block) => {
                        scope.enter_scope();
                        for (param, value) in block.params.positional.iter().zip(args.iter()) {
                            scope.add_var(param.0.name(), value[0].clone());
                        }
                        let result = run_block(&block, ctx, scope, input);
                        scope.exit_scope();

                        let result = result?;
                        return Ok(result);
                    }
                    Expression::Variable(v, span) => {
                        if let Some(value) = scope.get_var(v) {
                            match &value.value {
                                UntaggedValue::Block(captured_block) => {
                                    scope.enter_scope();
                                    scope.add_vars(&captured_block.captured.entries);
                                    for (param, value) in captured_block
                                        .block
                                        .params
                                        .positional
                                        .iter()
                                        .zip(args.iter())
                                    {
                                        scope.add_var(param.0.name(), value[0].clone());
                                    }
                                    let result =
                                        run_block(&captured_block.block, ctx, scope, input);
                                    scope.exit_scope();

                                    let result = result?;
                                    return Ok(result);
                                }
                                _ => {
                                    return Err(ShellError::labeled_error("Dynamic commands must start with a block (or variable pointing to a block)", "needs to be a block", call.head.span));
                                }
                            }
                        } else {
                            return Err(ShellError::labeled_error(
                                "Variable not found",
                                "variable not found",
                                span,
                            ));
                        }
                    }
                    _ => {
                        return Err(ShellError::labeled_error("Dynamic commands must start with a block (or variable pointing to a block)", "needs to be a block", call.head.span));
                    }
                }
            }

            ClassifiedCommand::Expr(expr) => run_expression_block(&*expr, ctx)?,

            ClassifiedCommand::Error(err) => return Err(err.into()),

            ClassifiedCommand::Internal(left) => run_internal_command(left, ctx, input)?,
        };
    }

    Ok(input)
}

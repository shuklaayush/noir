mod context;
mod value;

use context::SharedContext;
use iter_extended::vecmap;
use noirc_errors::Location;
use noirc_frontend::monomorphization::ast::{self, Expression, Program};

use self::{
    context::FunctionContext,
    value::{Tree, Values},
};

use super::{
    ir::{instruction::BinaryOp, types::Type, value::ValueId},
    ssa_builder::SharedBuilderContext,
};

pub(crate) fn generate_ssa(program: Program) {
    let context = SharedContext::new(program);
    let builder_context = SharedBuilderContext::default();

    let main = context.program.main();
    let mut function_context =
        FunctionContext::new(main.name.clone(), &main.parameters, &context, &builder_context);

    function_context.codegen_expression(&main.body);

    while let Some((src_function_id, _new_id)) = context.pop_next_function_in_queue() {
        let function = &context.program[src_function_id];
        // TODO: Need to ensure/assert the new function's id == new_id
        function_context.new_function(function.name.clone(), &function.parameters);
        function_context.codegen_expression(&function.body);
    }
}

impl<'a> FunctionContext<'a> {
    fn codegen_expression(&mut self, expr: &Expression) -> Values {
        match expr {
            Expression::Ident(ident) => self.codegen_ident(ident),
            Expression::Literal(literal) => self.codegen_literal(literal),
            Expression::Block(block) => self.codegen_block(block),
            Expression::Unary(unary) => self.codegen_unary(unary),
            Expression::Binary(binary) => self.codegen_binary(binary),
            Expression::Index(index) => self.codegen_index(index),
            Expression::Cast(cast) => self.codegen_cast(cast),
            Expression::For(for_expr) => self.codegen_for(for_expr),
            Expression::If(if_expr) => self.codegen_if(if_expr),
            Expression::Tuple(tuple) => self.codegen_tuple(tuple),
            Expression::ExtractTupleField(tuple, index) => {
                self.codegen_extract_tuple_field(tuple, *index)
            }
            Expression::Call(call) => self.codegen_call(call),
            Expression::Let(let_expr) => self.codegen_let(let_expr),
            Expression::Constrain(constrain, location) => {
                self.codegen_constrain(constrain, *location)
            }
            Expression::Assign(assign) => self.codegen_assign(assign),
            Expression::Semi(semi) => self.codegen_semi(semi),
        }
    }

    /// Codegen any non-tuple expression so that we can unwrap the Values
    /// tree to return a single value for use with most SSA instructions.
    fn codegen_non_tuple_expression(&mut self, expr: &Expression) -> ValueId {
        match self.codegen_expression(expr) {
            Tree::Branch(branches) => {
                panic!("codegen_non_tuple_expression called on tuple {branches:?}")
            }
            Tree::Leaf(value) => value.eval(),
        }
    }

    fn codegen_ident(&mut self, _ident: &ast::Ident) -> Values {
        todo!()
    }

    fn codegen_literal(&mut self, literal: &ast::Literal) -> Values {
        match literal {
            ast::Literal::Array(array) => {
                let elements = vecmap(&array.contents, |element| self.codegen_expression(element));
                let element_type = Self::convert_type(&array.element_type);
                self.codegen_array(elements, element_type)
            }
            ast::Literal::Integer(value, typ) => {
                let typ = Self::convert_non_tuple_type(typ);
                self.builder.numeric_constant(*value, typ).into()
            }
            ast::Literal::Bool(value) => {
                self.builder.numeric_constant(*value as u128, Type::bool()).into()
            }
            ast::Literal::Str(string) => {
                let elements = vecmap(string.as_bytes(), |byte| {
                    self.builder.numeric_constant(*byte as u128, Type::field()).into()
                });
                self.codegen_array(elements, Tree::Leaf(Type::field()))
            }
        }
    }

    fn codegen_array(&mut self, elements: Vec<Values>, element_type: Tree<Type>) -> Values {
        let size = element_type.size_of_type() * elements.len();
        let array = self.builder.insert_allocate(size.try_into().unwrap_or_else(|_| {
            panic!("Cannot allocate {size} bytes for array, it does not fit into a u32")
        }));

        // Now we must manually store all the elements into the array
        let mut i = 0u128;
        for element in elements {
            element.for_each(|value| {
                let address = self.make_offset(array, i);
                self.builder.insert_store(address, value.eval());
                i += 1;
            });
        }

        array.into()
    }

    fn codegen_block(&mut self, block: &[Expression]) -> Values {
        let mut result = self.unit_value();
        for expr in block {
            result = self.codegen_expression(expr);
        }
        result
    }

    fn codegen_unary(&mut self, unary: &ast::Unary) -> Values {
        let rhs = self.codegen_non_tuple_expression(&unary.rhs);
        match unary.operator {
            noirc_frontend::UnaryOp::Not => self.builder.insert_not(rhs).into(),
            noirc_frontend::UnaryOp::Minus => {
                let typ = self.builder.type_of_value(rhs);
                let zero = self.builder.numeric_constant(0u128, typ);
                self.builder.insert_binary(zero, BinaryOp::Sub, rhs).into()
            }
        }
    }

    fn codegen_binary(&mut self, binary: &ast::Binary) -> Values {
        let lhs = self.codegen_non_tuple_expression(&binary.lhs);
        let rhs = self.codegen_non_tuple_expression(&binary.rhs);
        self.insert_binary(lhs, binary.operator, rhs)
    }

    fn codegen_index(&mut self, index: &ast::Index) -> Values {
        let array = self.codegen_non_tuple_expression(&index.collection);
        let base_offset = self.codegen_non_tuple_expression(&index.index);

        // base_index = base_offset * type_size
        let type_size = Self::convert_type(&index.element_type).size_of_type();
        let type_size = self.builder.field_constant(type_size as u128);
        let base_index = self.builder.insert_binary(base_offset, BinaryOp::Mul, type_size);

        let mut field_index = 0u128;
        self.map_type(&index.element_type, |ctx, typ| {
            let offset = ctx.make_offset(base_index, field_index);
            field_index += 1;
            ctx.builder.insert_load(array, offset, typ).into()
        })
    }

    fn codegen_cast(&mut self, cast: &ast::Cast) -> Values {
        let lhs = self.codegen_non_tuple_expression(&cast.lhs);
        let typ = Self::convert_non_tuple_type(&cast.r#type);
        self.builder.insert_cast(lhs, typ).into()
    }

    fn codegen_for(&mut self, _for_expr: &ast::For) -> Values {
        todo!()
    }

    fn codegen_if(&mut self, if_expr: &ast::If) -> Values {
        let condition = self.codegen_non_tuple_expression(&if_expr.condition);

        let then_block = self.builder.insert_block();
        let else_block = self.builder.insert_block();

        self.builder.terminate_with_jmpif(condition, then_block, else_block);

        self.builder.switch_to_block(then_block);
        let then_value = self.codegen_expression(&if_expr.consequence);

        let mut result = self.unit_value();

        if let Some(alternative) = &if_expr.alternative {
            self.builder.switch_to_block(else_block);
            let else_value = self.codegen_expression(alternative);

            let end_block = self.builder.insert_block();

            // Create block arguments for the end block as needed to branch to
            // with our then and else value.
            result = self.map_type(&if_expr.typ, |ctx, typ| {
                ctx.builder.add_block_parameter(end_block, typ).into()
            });

            self.builder.terminate_with_jmp(end_block, else_value.into_value_list());

            // Must also set the then block to jmp to the end now
            self.builder.switch_to_block(then_block);
            self.builder.terminate_with_jmp(end_block, then_value.into_value_list());
            self.builder.switch_to_block(end_block);
        } else {
            // In the case we have no 'else', the 'else' block is actually the end block.
            self.builder.terminate_with_jmp(else_block, vec![]);
            self.builder.switch_to_block(else_block);
        }

        result
    }

    fn codegen_tuple(&mut self, tuple: &[Expression]) -> Values {
        Tree::Branch(vecmap(tuple, |expr| self.codegen_expression(expr)))
    }

    fn codegen_extract_tuple_field(&mut self, tuple: &Expression, index: usize) -> Values {
        match self.codegen_expression(tuple) {
            Tree::Branch(mut trees) => trees.remove(index),
            Tree::Leaf(value) => {
                unreachable!("Tried to extract tuple index {index} from non-tuple {value:?}")
            }
        }
    }

    fn codegen_call(&mut self, _call: &ast::Call) -> Values {
        todo!()
    }

    fn codegen_let(&mut self, _let_expr: &ast::Let) -> Values {
        todo!()
    }

    fn codegen_constrain(&mut self, expr: &Expression, _location: Location) -> Values {
        let boolean = self.codegen_non_tuple_expression(expr);
        self.builder.insert_constrain(boolean);
        self.unit_value()
    }

    fn codegen_assign(&mut self, _assign: &ast::Assign) -> Values {
        todo!()
    }

    fn codegen_semi(&mut self, expr: &Expression) -> Values {
        self.codegen_expression(expr);
        self.unit_value()
    }
}
// VHDL AST Pretty Printer
// Manually converts VHDL AST nodes back to VHDL source code strings
//
// Note: vhdl_lang provides VHDLFormatter, but its Buffer type is private,
// so we manually reconstruct expressions from the AST instead.

use vhdl_lang::ast::{Expression, Name, Designator, Literal, Operator};

/// Convert an Expression AST node to a VHDL string
pub fn expr_to_string(expr: &Expression) -> String {
    match expr {
        Expression::Literal(lit) => literal_to_string(lit),
        Expression::Name(name) => name_to_string(&**name),
        Expression::Binary(op, left, right) => {
            // Match on the binary operator enum directly - inline to avoid exposing private types
            let op_str = match &op.item.item {
                Operator::Plus => "+",
                Operator::Minus => "-",
                Operator::Times => "*",
                Operator::Div => "/",
                Operator::Mod => " mod ",
                Operator::Rem => " rem ",
                Operator::Pow => "**",
                Operator::And => " and ",
                Operator::Or => " or ",
                Operator::Nand => " nand ",
                Operator::Nor => " nor ",
                Operator::Xor => " xor ",
                Operator::Xnor => " xnor ",
                Operator::EQ => "=",
                Operator::NE => "/=",
                Operator::LT => "<",
                Operator::LTE => "<=",
                Operator::GT => ">",
                Operator::GTE => ">=",
                Operator::QueEQ => "?=",
                Operator::QueNE => "?/=",
                Operator::QueLT => "?<",
                Operator::QueLTE => "?<=",
                Operator::QueGT => "?>",
                Operator::QueGTE => "?>=",
                Operator::SLL => " sll ",
                Operator::SRL => " srl ",
                Operator::SLA => " sla ",
                Operator::SRA => " sra ",
                Operator::ROL => " rol ",
                Operator::ROR => " ror ",
                Operator::Concat => "&",
                _ => " ? ", // Catch-all for unexpected operators
            };
            format!("{} {} {}", expr_to_string(&left.item), op_str, expr_to_string(&right.item))
        },
        Expression::Unary(op, operand) => {
            let op_str = match &op.item.item {
                Operator::Plus => "+",
                Operator::Minus => "-",
                Operator::Not => "not ",
                Operator::Abs => "abs ",
                Operator::QueQue => "?? ",
                _ => "unary_op ",
            };
            format!("{}{}", op_str, expr_to_string(&operand.item))
        },
        Expression::Parenthesized(inner) => {
            format!("({})", expr_to_string(&inner.item))
        },
        _ => "complex_expr".to_string(),
    }
}

fn literal_to_string(lit: &Literal) -> String {
    match lit {
        Literal::AbstractLiteral(al) => al.to_string(),
        Literal::String(s) => format!("\"{}\"", s),
        Literal::BitString(bs) => format!("{:?}", bs),
        Literal::Character(c) => format!("'{}'", c),
        Literal::Null => "null".to_string(),
        Literal::Physical(val) => format!("{:?}", val),
    }
}

fn name_to_string(name: &Name) -> String {
    match name {
        Name::Designator(des) => {
            match &des.item {
                Designator::Identifier(sym) => sym.name_utf8(),
                Designator::OperatorSymbol(_) => "operator".to_string(),
                Designator::Character(c) => format!("'{}'", c),
                Designator::Anonymous(_) => "anonymous".to_string(),
            }
        },
        Name::Selected(prefix, suffix) => {
            let suffix_name = match &suffix.item.item {
                Designator::Identifier(sym) => sym.name_utf8(),
                _ => "suffix".to_string(),
            };
            format!("{}.{}", expr_to_string(&Expression::Name(Box::new(prefix.item.clone()))), suffix_name)
        },
        Name::SelectedAll(prefix) => {
            format!("{}.all", expr_to_string(&Expression::Name(Box::new(prefix.item.clone()))))
        },
        Name::CallOrIndexed(fcall) => {
            // For function calls like get_eth_hdr_bits(x)
            format!("{}", expr_to_string(&Expression::Name(Box::new(fcall.name.item.clone()))))
        },
        _ => "complex_name".to_string(),
    }
}

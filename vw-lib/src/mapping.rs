use vhdl_lang::ast::{
    AttributeSpecification, Designator, DiscreteRange, ElementDeclaration,
    EntityClass, EntityDeclaration, EntityName, Name, PackageDeclaration,
    Range, RangeConstraint, SubtypeConstraint, TypeDeclaration,
    TypeDefinition::Record,
};

use crate::visitor::{Visitor, VisitorResult};

const RECORD_PARSE_ATTRIBUTE: &str = "rust_me";

#[derive(Debug,Clone)]
pub enum VwSymbol {
    Package(String),
    Entity(String),
    Constant(String),
    Record(RecordData),
}

#[derive(Debug, Clone)]
pub struct RecordData {
    name: String,
    fields: Vec<FieldData>,
    tagged: bool,
}
impl RecordData {
    pub fn new(name: &str) -> Self {
        Self {
            name: String::from(name),
            fields: Vec::new(),
            tagged: false,
        }
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone)]
pub struct FieldData {
    pub name: String,
    pub subtype_name: String,
    pub constraint: Option<RangeConstraint>,
}

#[derive(Debug)]
pub struct VwSymbolFinder {
    symbols: Vec<VwSymbol>,
    records: Vec<RecordData>,
    tagged_types: Vec<String>,
    target_attr: String,
}

impl VwSymbolFinder {
    pub fn new(target_attr: &str) -> Self {
        Self {
            symbols: Vec::new(),
            records: Vec::new(),
            tagged_types: Vec::new(),
            target_attr: target_attr.to_string(),
        }
    }

    pub fn get_symbols(&self) -> &Vec<VwSymbol> {
        &self.symbols
    }

    pub fn get_records(&self) -> &Vec<RecordData> {
        &self.records
    }
    
    pub fn get_tagged_types(&self) -> &Vec<String> {
        &self.tagged_types
    }
}

impl Visitor for VwSymbolFinder {
    fn visit_attribute_specification(
        &mut self,
        spec: &AttributeSpecification,
    ) -> VisitorResult {
        if spec.ident.item.item.name_utf8() == self.target_attr {
            if let EntityClass::Type = spec.entity_class {
                if let EntityName::Name(tag) = &spec.entity_name {
                    if let Designator::Identifier(id) =
                        &tag.designator.item.item
                    {
                        let type_name = id.name_utf8();
                        self.tagged_types.push(type_name);
                    }
                }
            }
        }
        VisitorResult::Continue
    }

    fn visit_type_declaration(
        &mut self,
        decl: &TypeDeclaration,
    ) -> VisitorResult {
        if let Record(elements) = &decl.def {
            let name = decl.ident.tree.item.name_utf8();
            let mut record_struct = RecordData::new(&name);
            let fields = get_fields(elements);
            record_struct.fields = fields;
            self.records.push(record_struct);
        }
        VisitorResult::Continue
    }

    fn visit_entity(&mut self, entity: &EntityDeclaration) -> VisitorResult {
        let name = entity.ident.tree.item.name_utf8();
        self.symbols.push(VwSymbol::Entity(name));
        VisitorResult::Continue
    }

    fn visit_package(&mut self, package: &PackageDeclaration) -> VisitorResult {
        let name = package.ident.tree.item.name_utf8();
        self.symbols.push(VwSymbol::Package(name));
        VisitorResult::Continue
    }
}

fn get_fields(elements: &Vec<ElementDeclaration>) -> Vec<FieldData> {
    let mut fields = Vec::new();

    for element in elements {
        let element_name = element.idents[0].tree.item.name_utf8();
        let element_subtype = if let Name::Designator(designator) =
            &element.subtype.type_mark.item
        {
            if let Designator::Identifier(symbol) = &designator.item {
                Some(symbol.name_utf8())
            } else {
                None
            }
        } else {
            None
            // panic here for now, because i want to see what struct differences there
            // might be
        }
        .unwrap();

        let element_constraint =
            if let Some(constraint) = &element.subtype.constraint {
                Some(get_range_constraint(&constraint.item))
            } else {
                None
            };

        fields.push(FieldData {
            name: element_name,
            subtype_name: element_subtype,
            constraint: element_constraint,
        });
    }

    fields
}

fn get_range_constraint(constraint: &SubtypeConstraint) -> RangeConstraint {
    if let SubtypeConstraint::Array(array_range, _) = constraint {
        if let DiscreteRange::Range(discrete_range) = &array_range[0].item {
            if let Range::Range(constraint) = discrete_range {
                return constraint.clone();
            } else {
                panic!("We don't handle other range types")
            }
        } else {
            panic!("We don't handle other DiscreteRange types");
        }
    } else {
        panic!("We don't handle other constraint types");
    }
}

use vhdl_lang::ast::{
    AnyDesignUnit, AnyPrimaryUnit, AttributeSpecification, Designator,
    DiscreteRange, ElementDeclaration, EntityClass, EntityDeclaration,
    EntityName, Name, PackageDeclaration, Range, RangeConstraint,
    SubtypeConstraint, TypeDeclaration, TypeDefinition,
};

use crate::visitor::{Visitor, VisitorResult};

#[derive(Debug, Clone)]
pub enum VwSymbol {
    Package(String),
    Entity(String),
    Constant(String),
    Record(RecordData),
    Enum(EnumData),
}

#[derive(Debug, Clone)]
pub struct EnumData {
    pub containing_pkg: Option<String>,
    pub name: String,
    pub has_custom_encoding: bool,
}

impl EnumData {
    pub fn new(containing_pkg: Option<String>, name: &str) -> Self {
        Self {
            containing_pkg,
            name: String::from(name),
            has_custom_encoding: false,
        }
    }

    pub fn get_pkg_name(&self) -> Option<&String> {
        self.containing_pkg.as_ref()
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone)]
pub struct RecordData {
    containing_pkg: Option<String>,
    name: String,
    fields: Vec<FieldData>,
}

#[derive(Debug, Default)]
pub struct FileData {
    defined_pkgs: Vec<String>,
    imported_pkgs: Vec<String>,
}

impl FileData {
    pub fn new() -> Self {
        Self {
            defined_pkgs: Vec::new(),
            imported_pkgs: Vec::new(),
        }
    }

    pub fn add_defined_pkg(&mut self, pkg_name: &str) {
        self.defined_pkgs.push(pkg_name.to_string());
    }

    pub fn add_imported_pkg(&mut self, pkg_name: &str) {
        self.imported_pkgs.push(pkg_name.to_string());
    }

    pub fn get_imported_pkgs(&self) -> &Vec<String> {
        &self.imported_pkgs
    }
}

impl RecordData {
    pub fn new(containing_pkg: Option<String>, name: &str) -> Self {
        Self {
            containing_pkg,
            name: String::from(name),
            fields: Vec::new(),
        }
    }

    pub fn get_pkg_name(&self) -> Option<&String> {
        self.containing_pkg.as_ref()
    }

    pub fn get_fields(&self) -> &Vec<FieldData> {
        &self.fields
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
        _unit: &AnyDesignUnit,
    ) -> VisitorResult {
        let attr_name = spec.ident.item.item.name_utf8();

        // Check for custom enum encoding
        if attr_name == "enum_encoding" {
            if let EntityClass::Type = spec.entity_class {
                if let EntityName::Name(tag) = &spec.entity_name {
                    if let Designator::Identifier(id) =
                        &tag.designator.item.item
                    {
                        let type_name = id.name_utf8();
                        // Find the enum and set its flag
                        for symbol in &mut self.symbols {
                            if let VwSymbol::Enum(enum_data) = symbol {
                                if enum_data.name == type_name {
                                    enum_data.has_custom_encoding = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // if we found the attribute with the right name
        if attr_name == self.target_attr {
            // if we tagged a type (like a record)
            if let EntityClass::Type = spec.entity_class {
                // get the entity name
                if let EntityName::Name(tag) = &spec.entity_name {
                    // get the identifier
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

    #[allow(clippy::collapsible_match)]
    fn visit_type_declaration(
        &mut self,
        decl: &TypeDeclaration,
        unit: &AnyDesignUnit,
    ) -> VisitorResult {
        let name = decl.ident.tree.item.name_utf8();

        // Figure out where this type was defined (containing package)
        let defining_pkg_name =
            if let AnyDesignUnit::Primary(primary_unit) = unit {
                if let AnyPrimaryUnit::Package(package) = primary_unit {
                    Some(package.ident.tree.item.name_utf8())
                } else {
                    None
                }
            } else {
                None
            };

        match &decl.def {
            TypeDefinition::Record(elements) => {
                let mut record_struct =
                    RecordData::new(defining_pkg_name, &name);
                let fields = get_fields(elements);
                record_struct.fields = fields;
                self.records.push(record_struct);
            }
            TypeDefinition::Enumeration(_) => {
                let enum_data = EnumData::new(defining_pkg_name, &name);
                self.symbols.push(VwSymbol::Enum(enum_data));
            }
            _ => {}
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

        let element_constraint = element
            .subtype
            .constraint
            .as_ref()
            .map(|constraint| get_range_constraint(&constraint.item));

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
                constraint.clone()
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

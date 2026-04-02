use vhdl_lang::ast::{
    AnyDesignUnit, AnyPrimaryUnit, AttributeSpecification, Designator,
    DiscreteRange, ElementDeclaration, EntityClass, EntityDeclaration,
    EntityName, Name, ObjectClass, ObjectDeclaration, PackageDeclaration,
    PackageInstantiation, Range, RangeConstraint, SubtypeConstraint,
    TypeDeclaration, TypeDefinition,
};

use crate::visitor::{Visitor, VisitorResult};

#[derive(Debug, Clone)]
pub struct RecordFields {
    pub fields: Vec<FieldData>,
}

#[derive(Debug, Clone)]
pub struct EnumAttrs {
    pub has_custom_encoding: bool,
}

#[derive(Debug, Clone)]
pub enum SymbolKind {
    Package,
    Entity,
    Constant,
    Record(RecordFields),
    Enum(EnumAttrs),
}

#[derive(Debug, Clone)]
pub struct VwSymbol {
    pub containing_pkg: Option<String>,
    pub name: String,
    pub kind: SymbolKind,
}

impl VwSymbol {
    pub fn new(
        containing_pkg: Option<String>,
        name: &str,
        kind: SymbolKind,
    ) -> Self {
        Self {
            containing_pkg,
            name: String::from(name),
            kind,
        }
    }

    pub fn get_pkg_name(&self) -> Option<&String> {
        self.containing_pkg.as_ref()
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn get_fields(&self) -> Option<&Vec<FieldData>> {
        if let SymbolKind::Record(record) = &self.kind {
            Some(&record.fields)
        } else {
            None
        }
    }
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

#[derive(Debug, Clone)]
pub struct FieldData {
    pub name: String,
    pub subtype_name: String,
    pub constraint: Option<RangeConstraint>,
}

#[derive(Debug)]
pub struct VwSymbolFinder {
    symbols: Vec<VwSymbol>,
    tagged_types: Vec<String>,
    target_attr: String,
}

impl VwSymbolFinder {
    pub fn new(target_attr: &str) -> Self {
        Self {
            symbols: Vec::new(),
            tagged_types: Vec::new(),
            target_attr: target_attr.to_string(),
        }
    }

    pub fn get_symbols(&self) -> &Vec<VwSymbol> {
        &self.symbols
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
                            if let SymbolKind::Enum(attrs) = &mut symbol.kind
                            {
                                if symbol.name == type_name {
                                    attrs.has_custom_encoding = true;
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
            match spec.entity_class {
                EntityClass::Type | EntityClass::Constant => {
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
                },
                _ => {}
            }
        }
        VisitorResult::Continue
    }

    fn visit_object_declaration(
        &mut self,
        decl: &ObjectDeclaration,
        unit: &AnyDesignUnit,
    ) -> VisitorResult {
        // if this is a constant Declaration
        if let ObjectClass::Constant = decl.class {
            let const_name = decl.idents[0].tree.item.name_utf8();
            // where was this constant defined
            let def_pkg_name = if let AnyDesignUnit::Primary(
                AnyPrimaryUnit::Package(package),
            ) = unit
            {
                Some(package.ident.tree.item.name_utf8())
            } else {
                None
            };
            self.symbols.push(VwSymbol::new(
                def_pkg_name,
                &const_name,
                SymbolKind::Constant,
            ));
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
                let fields = get_fields(elements);
                self.symbols.push(VwSymbol::new(
                    defining_pkg_name,
                    &name,
                    SymbolKind::Record(RecordFields { fields }),
                ));
            }
            TypeDefinition::Enumeration(_) => {
                self.symbols.push(VwSymbol::new(
                    defining_pkg_name,
                    &name,
                    SymbolKind::Enum(EnumAttrs {
                        has_custom_encoding: false,
                    }),
                ));
            }
            _ => {}
        }
        VisitorResult::Continue
    }

    fn visit_entity(&mut self, entity: &EntityDeclaration) -> VisitorResult {
        let name = entity.ident.tree.item.name_utf8();
        self.symbols
            .push(VwSymbol::new(None, &name, SymbolKind::Entity));
        VisitorResult::Continue
    }

    fn visit_package(&mut self, package: &PackageDeclaration) -> VisitorResult {
        let name = package.ident.tree.item.name_utf8();
        self.symbols
            .push(VwSymbol::new(None, &name, SymbolKind::Package));
        VisitorResult::Continue
    }

    fn visit_package_instance(
        &mut self,
        instance: &PackageInstantiation,
    ) -> VisitorResult {
        let name = instance.ident.tree.item.name_utf8();
        self.symbols
            .push(VwSymbol::new(None, &name, SymbolKind::Package));
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

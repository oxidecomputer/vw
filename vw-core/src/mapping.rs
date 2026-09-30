use vhdl_lang::ast::{
    AnyDesignUnit, AnyPrimaryUnit, ArrayIndex, AttributeSpecification,
    Designator, DiscreteRange, ElementDeclaration, EntityClass,
    EntityDeclaration, EntityName, Expression, Name, ObjectClass,
    ObjectDeclaration, PackageDeclaration, PackageInstantiation, Range,
    RangeConstraint, SubtypeConstraint, TypeDeclaration, TypeDefinition,
};

use crate::visitor::{Visitor, VisitorResult};

#[derive(Debug, Clone)]
pub struct ConstantExpr {
    pub type_name: Name,
    pub expression: Option<Expression>,
}

#[derive(Debug, Clone)]
pub struct RecordFields {
    pub fields: Vec<FieldData>,
}

#[derive(Debug, Clone)]
pub struct EnumAttrs {
    pub has_custom_encoding: bool,
}

/// A subtype declaration: `subtype beat is std_logic_vector(511 downto 0)`.
///
/// Kept as what was written rather than resolved here, because resolving it
/// means following a chain — `quad_segments` is a `segment_vector` is an
/// array of `segment` is a `std_logic_vector` — and the chain can only be
/// walked once every link has been collected.
#[derive(Debug, Clone)]
pub struct SubtypeData {
    /// The type mark it was declared from.
    pub type_mark: String,
    /// Its constraint, if it narrowed one.
    pub constraint: Option<RangeConstraint>,
}

/// An array type: `type segment_vector is array(natural range <>) of segment`.
#[derive(Debug, Clone)]
pub struct ArrayData {
    /// The element's type mark.
    pub element_type: String,
    /// A constraint written on the element's subtype indication, as in
    /// `array(natural range <>) of std_logic_vector(7 downto 0)`.
    pub element_constraint: Option<RangeConstraint>,
    /// The index range, when the declaration gives one. Unconstrained for the
    /// usual `array(natural range <>)`, in which case a subtype of this one
    /// supplies it.
    pub index_constraint: Option<RangeConstraint>,
}

#[derive(Debug, Clone)]
pub enum SymbolKind {
    Package,
    Entity,
    Constant(ConstantExpr),
    Record(RecordFields),
    Enum(EnumAttrs),
    Subtype(SubtypeData),
    Array(ArrayData),
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
                            if let SymbolKind::Enum(attrs) = &mut symbol.kind {
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
                // `Subtype` as well as `Type`: `attribute serialize_rust of
                // beat : subtype is true` is how a subtype is tagged, and
                // dropping it here made the tag a silent no-op — the
                // generator ran, reported success, and emitted nothing.
                EntityClass::Type
                | EntityClass::Subtype
                | EntityClass::Constant => {
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

            // figure out its expression. VHDL 2019 widened the
            // initializer to a `ConditionalExpression`; we only
            // ever consumed the simple form here (record-field
            // defaults etc.), so unwrap `Simple` and drop
            // conditional forms as if the constant had no
            // initializer.
            let expr =
                decl.expression.as_ref().and_then(|span| match &span.item {
                    vhdl_lang::ast::ConditionalExpression::Simple(e) => {
                        Some(e.clone())
                    }
                    vhdl_lang::ast::ConditionalExpression::Conditional(_) => {
                        None
                    }
                });
            let type_name = decl.subtype_indication.type_mark.item.clone();

            self.symbols.push(VwSymbol::new(
                def_pkg_name,
                &const_name,
                SymbolKind::Constant(ConstantExpr {
                    type_name,
                    expression: expr,
                }),
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
            TypeDefinition::Subtype(indication) => {
                if let Some(type_mark) =
                    type_mark_name(&indication.type_mark.item)
                {
                    self.symbols.push(VwSymbol::new(
                        defining_pkg_name,
                        &name,
                        SymbolKind::Subtype(SubtypeData {
                            type_mark,
                            constraint: indication
                                .constraint
                                .as_ref()
                                .and_then(|c| range_constraint(&c.item)),
                        }),
                    ));
                }
            }
            TypeDefinition::Array(indexes, _, element) => {
                // One index only. A multi-dimensional array has no single
                // element count, and nothing downstream knows what to do with
                // one — better absent from the symbol table than present and
                // wrong.
                if indexes.len() != 1 {
                    return VisitorResult::Continue;
                }
                if let Some(element_type) =
                    type_mark_name(&element.type_mark.item)
                {
                    self.symbols.push(VwSymbol::new(
                        defining_pkg_name,
                        &name,
                        SymbolKind::Array(ArrayData {
                            element_type,
                            element_constraint: element
                                .constraint
                                .as_ref()
                                .and_then(|c| range_constraint(&c.item)),
                            index_constraint: match &indexes[0] {
                                ArrayIndex::Discrete(range) => {
                                    discrete_range_constraint(&range.item)
                                }
                                // `natural range <>` — a subtype of this
                                // supplies the bound.
                                ArrayIndex::IndexSubtypeDefintion(_) => None,
                            },
                        }),
                    ));
                }
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
        let Some(element_subtype) =
            type_mark_name(&element.subtype.type_mark.item)
        else {
            continue;
        };

        let element_constraint = element
            .subtype
            .constraint
            .as_ref()
            .and_then(|constraint| range_constraint(&constraint.item));

        // Every name, not just the first. `eop, valid, ready : std_logic;`
        // declares three elements, and taking one of them produced a struct
        // two bits narrower than the record it was generated from — which
        // round-trips against itself and disagrees with the simulator.
        for ident in &element.idents {
            fields.push(FieldData {
                name: ident.tree.item.name_utf8(),
                subtype_name: element_subtype.clone(),
                constraint: element_constraint.clone(),
            });
        }
    }

    fields
}

/// The name a type mark refers to, unqualified.
///
/// `work.eth_types.segment` and `segment` name the same type, and callers only
/// ever match on the last segment. Returns `None` for a mark that is not a
/// name at all — an attribute or a slice — which is not something anything
/// downstream can use, and which used to be an `unwrap` and a panic.
pub fn type_mark_name(name: &Name) -> Option<String> {
    match name {
        Name::Designator(designator) => match &designator.item {
            Designator::Identifier(symbol) => Some(symbol.name_utf8()),
            _ => None,
        },
        Name::Selected(_, suffix) => match &suffix.item.item {
            Designator::Identifier(symbol) => Some(symbol.name_utf8()),
            _ => None,
        },
        _ => None,
    }
}

/// The range a subtype constraint gives, if it gives a simple one.
///
/// Returns `None` rather than panicking for everything else — a record
/// constraint, an attribute range (`t'range`), a non-array constraint like
/// `integer range 0 to 7`. A caller that needs a range and did not get one
/// can say so about the field it was looking at; a panic here could only say
/// which line of this file it happened on.
pub fn range_constraint(
    constraint: &SubtypeConstraint,
) -> Option<RangeConstraint> {
    let SubtypeConstraint::Array(array_range, _) = constraint else {
        return None;
    };
    discrete_range_constraint(&array_range.first()?.item)
}

/// The same, for one discrete range.
pub fn discrete_range_constraint(
    range: &DiscreteRange,
) -> Option<RangeConstraint> {
    let DiscreteRange::Range(range) = range else {
        return None;
    };
    match range {
        Range::Range(constraint) => Some(constraint.clone()),
        // `t'range` and the like: real VHDL, but it carries no literal bounds
        // to read here.
        Range::Attribute(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visitor::walk_design_file;
    use vhdl_lang::{VHDLParser, VHDLStandard};

    /// Everything the symbol finder made of one package.
    fn symbols(source: &str) -> Vec<VwSymbol> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("types.vhd");
        std::fs::write(&file, source).unwrap();

        let parser = VHDLParser::new(VHDLStandard::VHDL2019);
        let mut diagnostics = Vec::new();
        let (_, design_file) =
            parser.parse_design_file(&file, &mut diagnostics).unwrap();

        let mut finder = VwSymbolFinder::new("serialize_rust");
        walk_design_file(&mut finder, &design_file);
        finder.get_symbols().clone()
    }

    fn tagged(source: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("types.vhd");
        std::fs::write(&file, source).unwrap();

        let parser = VHDLParser::new(VHDLStandard::VHDL2019);
        let mut diagnostics = Vec::new();
        let (_, design_file) =
            parser.parse_design_file(&file, &mut diagnostics).unwrap();

        let mut finder = VwSymbolFinder::new("serialize_rust");
        walk_design_file(&mut finder, &design_file);
        finder.get_tagged_types().clone()
    }

    fn find<'a>(symbols: &'a [VwSymbol], name: &str) -> &'a VwSymbol {
        symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name}"))
    }

    const TYPES: &str = "
package p is
  attribute serialize_rust : boolean;

  subtype beat is std_logic_vector(511 downto 0);
  attribute serialize_rust of beat : subtype is true;

  subtype segment is std_logic_vector(127 downto 0);
  type segment_vector is array(natural range <>) of segment;
  subtype quad_segments is segment_vector(3 downto 0);
  attribute serialize_rust of quad_segments : subtype is true;

  type flags is record
    eop, valid, ready : std_logic;
    data : work.p.beat;
  end record;
  attribute serialize_rust of flags : type is true;
end package;
";

    /// A subtype declaration is a symbol. Without one, a record field typed
    /// with a subtype cannot be resolved to anything at all.
    #[test]
    fn subtype_declarations_are_collected() {
        let symbols = symbols(TYPES);
        let SymbolKind::Subtype(data) = &find(&symbols, "beat").kind else {
            panic!("beat is not a subtype");
        };
        assert_eq!(data.type_mark, "std_logic_vector");
        assert!(data.constraint.is_some(), "511 downto 0");
    }

    /// So is an array type, along with what it is an array of.
    #[test]
    fn array_declarations_are_collected() {
        let symbols = symbols(TYPES);
        let SymbolKind::Array(data) = &find(&symbols, "segment_vector").kind
        else {
            panic!("segment_vector is not an array");
        };
        assert_eq!(data.element_type, "segment");
        assert!(
            data.index_constraint.is_none(),
            "`natural range <>` is unconstrained; a subtype supplies the bound",
        );

        // ...and the subtype that does supply it.
        let SymbolKind::Subtype(data) = &find(&symbols, "quad_segments").kind
        else {
            panic!("quad_segments is not a subtype");
        };
        assert_eq!(data.type_mark, "segment_vector");
        assert!(data.constraint.is_some(), "3 downto 0");
    }

    /// `attribute serialize_rust of beat : subtype is true` is how a subtype
    /// is tagged. Dropping the tag made it a silent no-op — the generator ran,
    /// reported success, and emitted nothing for it.
    #[test]
    fn a_tag_on_a_subtype_is_a_tag() {
        let tagged = tagged(TYPES);
        assert!(tagged.contains(&"beat".to_string()));
        assert!(tagged.contains(&"quad_segments".to_string()));
        assert!(tagged.contains(&"flags".to_string()));
    }

    /// One declaration naming three elements is three elements. Taking only
    /// the first produced a record two bits narrower than the VHDL it came
    /// from — which round-trips against itself and disagrees with the
    /// simulator, so nothing ever fails to say so.
    #[test]
    fn every_name_in_a_declaration_is_a_field() {
        let symbols = symbols(TYPES);
        let SymbolKind::Record(record) = &find(&symbols, "flags").kind else {
            panic!("flags is not a record");
        };
        let names: Vec<&str> =
            record.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["eop", "valid", "ready", "data"]);
        // The shared subtype reaches all three.
        assert_eq!(record.fields[1].subtype_name, "std_logic");
    }

    /// A qualified type mark names the same type as a bare one, and used to
    /// panic on an `unwrap`.
    #[test]
    fn a_qualified_type_mark_is_read_as_its_last_segment() {
        let symbols = symbols(TYPES);
        let SymbolKind::Record(record) = &find(&symbols, "flags").kind else {
            panic!("flags is not a record");
        };
        let data = record.fields.iter().find(|f| f.name == "data").unwrap();
        assert_eq!(data.subtype_name, "beat", "`work.p.beat` is `beat`");
    }

    /// A constraint that is not an array range is not one this can read, and
    /// saying so beats panicking inside a visitor.
    #[test]
    fn an_unreadable_constraint_is_absent_rather_than_fatal() {
        let symbols = symbols(
            "
package p is
  type odd is record
    count : integer range 0 to 7;
    data  : std_logic_vector(7 downto 0);
  end record;
end package;
",
        );
        let SymbolKind::Record(record) = &find(&symbols, "odd").kind else {
            panic!("odd is not a record");
        };
        assert!(record.fields[0].constraint.is_none(), "integer range");
        assert!(record.fields[1].constraint.is_some(), "7 downto 0");
    }
}

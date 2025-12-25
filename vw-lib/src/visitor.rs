//! Generic AST visitor for vhdl_lang.
//!
//! This module provides a `Visitor` trait that allows arbitrary AST traversal,
//! unlike the built-in `Searcher` trait which only exposes limited node types.
//!
//! # Example
//!
//! ```ignore
//! use vw_lib::visitor::{Visitor, VisitorResult, walk_design_file};
//!
//! struct MyVisitor {
//!     record_count: usize,
//! }
//!
//! impl Visitor for MyVisitor {
//!     fn visit_type_declaration(&mut self, decl: &TypeDeclaration) -> VisitorResult {
//!         if matches!(&decl.def, TypeDefinition::Record(_)) {
//!             self.record_count += 1;
//!         }
//!         VisitorResult::Continue
//!     }
//! }
//! ```

use vhdl_lang::ast::{
    AnyDesignUnit, AnyPrimaryUnit, AnySecondaryUnit,
    ArchitectureBody, Attribute, AttributeDeclaration, AttributeSpecification,
    ComponentDeclaration, ConfigurationDeclaration, ContextDeclaration,
    Declaration, DesignFile, EntityDeclaration,
    PackageBody, PackageDeclaration, PackageInstantiation,
    SubprogramBody, SubprogramDeclaration, SubprogramInstantiation,
    TypeDeclaration,
};

/// Controls whether AST traversal should continue or stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisitorResult {
    /// Continue traversing the AST
    Continue,
    /// Stop traversal immediately
    Stop,
}

impl VisitorResult {
    /// Returns true if traversal should continue
    pub fn should_continue(&self) -> bool {
        matches!(self, VisitorResult::Continue)
    }
}

/// A trait for visiting nodes in a vhdl_lang AST.
///
/// All methods have default implementations that return `Continue`,
/// so you only need to override the methods for nodes you care about.
///
/// Methods are called in a depth-first traversal order.
#[allow(unused_variables)]
pub trait Visitor {
    // ========================================================================
    // Design Units
    // ========================================================================

    /// Called for each design file before visiting its contents
    fn visit_design_file(&mut self, file: &DesignFile) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for each design unit
    fn visit_design_unit(&mut self, unit: &AnyDesignUnit) -> VisitorResult {
        VisitorResult::Continue
    }

    // ------------------------------------------------------------------------
    // Primary Units
    // ------------------------------------------------------------------------

    /// Called for entity declarations
    fn visit_entity(&mut self, entity: &EntityDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for package declarations
    fn visit_package(&mut self, package: &PackageDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for package instantiations
    fn visit_package_instance(&mut self, instance: &PackageInstantiation) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for context declarations
    fn visit_context(&mut self, context: &ContextDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for configuration declarations
    fn visit_configuration(&mut self, config: &ConfigurationDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    // ------------------------------------------------------------------------
    // Secondary Units
    // ------------------------------------------------------------------------

    /// Called for architecture bodies
    fn visit_architecture(&mut self, arch: &ArchitectureBody) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for package bodies
    fn visit_package_body(&mut self, body: &PackageBody) -> VisitorResult {
        VisitorResult::Continue
    }

    // ========================================================================
    // Declarations
    // ========================================================================

    /// Called for each declaration (before dispatching to specific type)
    fn visit_declaration(&mut self, decl: &Declaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for type declarations
    fn visit_type_declaration(&mut self, decl: &TypeDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for component declarations
    fn visit_component(&mut self, comp: &ComponentDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for subprogram declarations (function/procedure specs)
    fn visit_subprogram_declaration(&mut self, decl: &SubprogramDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for subprogram bodies (function/procedure implementations)
    fn visit_subprogram_body(&mut self, body: &SubprogramBody) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for subprogram instantiations
    fn visit_subprogram_instantiation(&mut self, inst: &SubprogramInstantiation) -> VisitorResult {
        VisitorResult::Continue
    }

    // ------------------------------------------------------------------------
    // Attributes
    // ------------------------------------------------------------------------

    /// Called for attribute declarations (attribute X : type)
    fn visit_attribute_declaration(&mut self, decl: &AttributeDeclaration) -> VisitorResult {
        VisitorResult::Continue
    }

    /// Called for attribute specifications (attribute X of Y : class is value)
    fn visit_attribute_specification(&mut self, spec: &AttributeSpecification) -> VisitorResult {
        VisitorResult::Continue
    }
}

/// Walk a design file, calling visitor methods for each node.
pub fn walk_design_file<V: Visitor>(visitor: &mut V, file: &DesignFile) -> VisitorResult {
    if !visitor.visit_design_file(file).should_continue() {
        return VisitorResult::Stop;
    }

    for (_tokens, unit) in &file.design_units {
        if !walk_design_unit(visitor, unit).should_continue() {
            return VisitorResult::Stop;
        }
    }

    VisitorResult::Continue
}

/// Walk a design unit, calling visitor methods for each node.
pub fn walk_design_unit<V: Visitor>(visitor: &mut V, unit: &AnyDesignUnit) -> VisitorResult {
    if !visitor.visit_design_unit(unit).should_continue() {
        return VisitorResult::Stop;
    }

    match unit {
        AnyDesignUnit::Primary(primary) => walk_primary_unit(visitor, primary),
        AnyDesignUnit::Secondary(secondary) => walk_secondary_unit(visitor, secondary),
    }
}

/// Walk a primary unit.
fn walk_primary_unit<V: Visitor>(visitor: &mut V, unit: &AnyPrimaryUnit) -> VisitorResult {
    match unit {
        AnyPrimaryUnit::Entity(entity) => {
            if !visitor.visit_entity(entity).should_continue() {
                return VisitorResult::Stop;
            }
            walk_declarations(visitor, &entity.decl)
        }
        AnyPrimaryUnit::Package(package) => {
            if !visitor.visit_package(package).should_continue() {
                return VisitorResult::Stop;
            }
            walk_declarations(visitor, &package.decl)
        }
        AnyPrimaryUnit::PackageInstance(instance) => {
            visitor.visit_package_instance(instance)
        }
        AnyPrimaryUnit::Context(context) => {
            visitor.visit_context(context)
        }
        AnyPrimaryUnit::Configuration(config) => {
            visitor.visit_configuration(config)
        }
    }
}

/// Walk a secondary unit.
fn walk_secondary_unit<V: Visitor>(visitor: &mut V, unit: &AnySecondaryUnit) -> VisitorResult {
    match unit {
        AnySecondaryUnit::Architecture(arch) => {
            if !visitor.visit_architecture(arch).should_continue() {
                return VisitorResult::Stop;
            }
            walk_declarations(visitor, &arch.decl)
        }
        AnySecondaryUnit::PackageBody(body) => {
            if !visitor.visit_package_body(body).should_continue() {
                return VisitorResult::Stop;
            }
            walk_declarations(visitor, &body.decl)
        }
    }
}

/// Walk a list of declarations.
fn walk_declarations<V: Visitor>(
    visitor: &mut V,
    decls: &[vhdl_lang::ast::token_range::WithTokenSpan<Declaration>],
) -> VisitorResult {
    for decl in decls {
        if !walk_declaration(visitor, &decl.item).should_continue() {
            return VisitorResult::Stop;
        }
    }
    VisitorResult::Continue
}

/// Walk a single declaration.
fn walk_declaration<V: Visitor>(visitor: &mut V, decl: &Declaration) -> VisitorResult {
    // First call the generic declaration visitor
    if !visitor.visit_declaration(decl).should_continue() {
        return VisitorResult::Stop;
    }

    // Then dispatch to specific visitors
    match decl {
        Declaration::Type(type_decl) => {
            visitor.visit_type_declaration(type_decl)
        }
        Declaration::Component(comp) => {
            visitor.visit_component(comp)
        }
        Declaration::Attribute(attr) => {
            match attr {
                Attribute::Declaration(decl) => {
                    visitor.visit_attribute_declaration(decl)
                }
                Attribute::Specification(spec) => {
                    visitor.visit_attribute_specification(spec)
                }
            }
        }
        Declaration::SubprogramDeclaration(decl) => {
            visitor.visit_subprogram_declaration(decl)
        }
        Declaration::SubprogramBody(body) => {
            if !visitor.visit_subprogram_body(body).should_continue() {
                return VisitorResult::Stop;
            }
            // Recurse into subprogram body declarations
            walk_declarations(visitor, &body.declarations)
        }
        Declaration::SubprogramInstantiation(inst) => {
            visitor.visit_subprogram_instantiation(inst)
        }
        // For other declaration types, just continue
        _ => VisitorResult::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingVisitor {
        entities: usize,
        packages: usize,
        types: usize,
        attr_specs: usize,
    }

    impl CountingVisitor {
        fn new() -> Self {
            Self {
                entities: 0,
                packages: 0,
                types: 0,
                attr_specs: 0,
            }
        }
    }

    impl Visitor for CountingVisitor {
        fn visit_entity(&mut self, _: &EntityDeclaration) -> VisitorResult {
            self.entities += 1;
            VisitorResult::Continue
        }

        fn visit_package(&mut self, _: &PackageDeclaration) -> VisitorResult {
            self.packages += 1;
            VisitorResult::Continue
        }

        fn visit_type_declaration(&mut self, _: &TypeDeclaration) -> VisitorResult {
            self.types += 1;
            VisitorResult::Continue
        }

        fn visit_attribute_specification(&mut self, _: &AttributeSpecification) -> VisitorResult {
            self.attr_specs += 1;
            VisitorResult::Continue
        }
    }
}

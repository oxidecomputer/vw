use std::{
    collections::HashMap,
    fs,
};

use crate::{RecordProcessor, VhdlStandard, nvc_helpers::{run_nvc_elab, run_nvc_sim}};
use crate::VwError;
use crate::vhdl_printer::expr_to_string;
use crate::nvc_helpers::run_nvc_analysis;

use quote::{quote, format_ident};
use proc_macro2::TokenStream;

use vhdl_lang::ast::{
    AbstractLiteral, 
    Expression, 
    Literal,
};

enum Side {
    Left,
    Right 
}

/// struct for identifying a particular
/// range constraint within a field with a struct
struct ConstraintID {
    record_index : usize,
    field_index : usize,
    side : Side
}


#[derive(Debug, Clone, Default)]
pub struct ResolvedRange {
    left : Option<usize>,
    right : Option<usize>
}

#[derive(Debug, Clone)]
pub struct ResolvedField {
    pub name: String,
    pub bit_width: Option<ResolvedRange>,
    pub subtype_name: String,
}
impl ResolvedField {
    pub fn new(name : &str, subtype : &str, bitwidth : Option<ResolvedRange>) -> Self {
        ResolvedField { 
            name: name.to_string(),
            bit_width: bitwidth, 
            subtype_name: subtype.to_string() 
        }
    }
}

// Resolved record with all field widths computed
#[derive(Debug, Clone)]
pub struct ResolvedRecord {
    pub name: String,
    pub fields: Vec<ResolvedField>,
}

impl ResolvedRecord {
    pub fn new(name: &str) -> Self {
        ResolvedRecord { 
            name: name.to_string(), 
            fields: Vec::new() 
        }
    }
}

pub async fn anodize_records(
    processor : &RecordProcessor, 
    referenced_files : &Vec<String>,
    generate_dir : String,
    build_dir : String,
    rust_out_dir : String
) -> Result<(), VwError>{
    let mut tagged_records = Vec::new();
    let mut packages_needed = Vec::new();
    for name in &processor.tagged_names {
        match processor.records.get(name) {
            Some(record) => {
                tagged_records.push(record);
                let pkg_name = record.get_pkg_name()
                    .ok_or_else(||
                    VwError::CodeGen { 
                        message: format!("Serialization not supported for \
                        records not in packages. Record : {:}", record.get_name()
                        )
                    }
                )?;
                packages_needed.push(pkg_name.clone());
            }
            None => {
                return Err(VwError::CodeGen { 
                    message: format!("Tagged type with name {:} not supported", name) 
                })
            }
        }
    }
    
    let mut expr_to_resolve : HashMap<String, Vec<ConstraintID>> = HashMap::new();
    let mut process_records = Vec::new();

    // ok we got tagged records. time to collect either their subtypes or their constraints
    for (i, record) in tagged_records.iter().enumerate() {
        let mut record_resolution = ResolvedRecord::new(record.get_name());
        for (j, field) in record.get_fields().iter().enumerate() {
            // possibly FIXME: work for non-ASCII strings?
            if field.subtype_name.eq_ignore_ascii_case("std_logic_vector") {
                let mut resolve_range = ResolvedRange::default(); 
                // ok we may need to resolve either the left or right range constraints
                if let Some(range) = &field.constraint {
                    // check the left constraint
                    // is it possible to immediately derive a value?
                    if let Expression::Literal(
                        Literal::AbstractLiteral(
                        AbstractLiteral::Integer(value)
                    )) = range.left_expr.item {
                        // unwrap here because we just put the range in the field
                        resolve_range.left = Some(value as usize);
                    }
                    // ok we have to have VHDL evaluate it
                    else {
                        let expr_str = expr_to_string(&range.left_expr.item);
                        expr_to_resolve.entry(expr_str).or_default().push(
                            ConstraintID { 
                                record_index: i,
                                field_index: j,
                                side: Side::Left 
                            }
                        );
                    }
                    // check the right constraint
                    if let Expression::Literal(
                        Literal::AbstractLiteral(
                            AbstractLiteral::Integer(value)
                        )) = range.right_expr.item {
                        resolve_range.right = Some(value as usize);
                    }
                    else {
                        let expr_str = expr_to_string(&range.right_expr.item);
                        expr_to_resolve.entry(
                            expr_str
                        ).or_default().push(
                            ConstraintID { 
                                record_index: i,
                                field_index: j, 
                                side: Side::Right
                            }
                        );
                    }
                    let resolve_field = ResolvedField::new(
                        &field.name, 
                        &field.subtype_name,
                        Some(resolve_range)
                    );
                    record_resolution.fields.push(resolve_field);
                }
                else {
                    return Err(VwError::CodeGen { 
                        message: format!("All fields in serialized structs must be constrained. \
                         Found unconstrained field {:} in record {:}", field.name, record.get_name()
                        )
                    });
                }
            }
            else if field.subtype_name.eq_ignore_ascii_case("std_logic") {
                let mut range = ResolvedRange::default();
                range.left = Some(0);
                range.right = Some(0);
                record_resolution.fields.push(ResolvedField::new(
                    &field.name,
                    &field.subtype_name,
                    Some(range)
                )); 
            }
            // make sure the subtype struct is captured too
            else {
                if !processor.tagged_names.contains(&field.subtype_name) {
                    return Err(VwError::CodeGen { 
                        message: format!("Subtype {:} not tagged for serialization. Please tag it", field.subtype_name)
                    });
                }
                else {
                    record_resolution.fields.push(ResolvedField::new(
                        &field.name,
                        &field.subtype_name,
                    None
                    ));
                }
            }
        }
        process_records.push(record_resolution);
    }


    // alright, we've collected all the expressions that need resolving...create a testbench
    let exprs = expr_to_resolve.keys().cloned().collect();
    let testbench = create_testbench(&exprs, &packages_needed);

    let generate_path = format!("{}/{}", build_dir, generate_dir);

    fs::create_dir_all(generate_path.clone())?;
    fs::write(format!("{}/constraint_tb.vhd", generate_path), &testbench)?;

    let tb_files : Vec<String> = referenced_files.iter().cloned()
        .chain(std::iter::once(format!("{}/constraint_tb.vhd", generate_path)))
        .collect();

    // ok and now we have to run the testbench
    // analyze the testbench
    let (std_out_analysis, std_err_analysis) = run_nvc_analysis(
        VhdlStandard::Vhdl2019,
        &build_dir,
        &"generated".to_string(),
        &tb_files,
        true
    ).await?.unwrap();

    let stdout_a_path = format!("{}/analysis.out", generate_path);
    let stderr_a_path = format!("{}/analysis.err", generate_path);
    fs::write(stdout_a_path, &std_out_analysis)?;
    fs::write(stderr_a_path, &std_err_analysis)?;

    //elaborate the testbench
    let (stdout_elab, stderr_elab) = run_nvc_elab(
        VhdlStandard::Vhdl2019, 
        &build_dir, 
        &"generated".to_string(), 
        &"constraint_evaluator".to_string(), 
        true).await?.unwrap();

    let stdout_e_path = format!("{}/elab.out", generate_path);
    let stderr_e_path = format!("{}/elab.err", generate_path);
    fs::write(stdout_e_path, &stdout_elab)?;
    fs::write(stderr_e_path, &stderr_elab)?;


    // run the testbench

    let (stdout_sim, stderr_sim) = run_nvc_sim(
        VhdlStandard::Vhdl2019, 
        &build_dir, 
        &"generated".to_string(), 
        &"constraint_evaluator".to_string(), 
        None, 
        &Vec::new(), 
        true
    ).await?.unwrap();

    let stdout_sim_path = format!("{}/sim.out", generate_path);
    let stderr_sim_path = format!("{}/sim.err", generate_path);

    fs::write(stdout_sim_path, &stdout_sim)?;
    fs::write(stderr_sim_path, &stderr_sim)?;

    // process the sim output to resolve the expressions
    process_sim_output(
        &exprs, 
        expr_to_resolve, 
        &mut process_records, 
        &stdout_sim
    )?;
    

    // ok generate Rust code from the resolved records
    let rust_content = generate_rust_structs(&process_records)?;
    let rust_structs_file = format!("{}/generated_structs.rs", rust_out_dir);
    fs::write(rust_structs_file, rust_content)?;

    Ok(())
}

fn generate_rust_structs(resolved_recs: &Vec<ResolvedRecord>) -> Result<String, VwError> {
    let structs: Result<Vec<TokenStream>, VwError> = resolved_recs.iter().map(|record| {
        let struct_name = format_ident!("{}", record.name);

        let fields: Result<Vec<TokenStream>, VwError> = record.fields.iter().map(|field| {
            let field_name = format_ident!("{}", field.name);

            if let Some(range) = &field.bit_width {
                let left = range.left.ok_or_else(|| VwError::CodeGen {
                    message: format!("Somehow didn't resolve left expression for field {:}", field.name)
                })?;
                let right = range.right.ok_or_else(|| VwError::CodeGen {
                    message: format!("Somehow didn't resolve right expression for field {:}", field.name)
                })?;
                let bitwidth = left - right + 1;

                Ok(quote! {
                    pub #field_name: BitfieldWrap<#bitwidth>
                })
            } else {
                let subtype = format_ident!("{}", field.subtype_name);
                Ok(quote! {
                    pub #field_name: #subtype
                })
            }
        }).collect();

        let fields = fields?;

        let constructor_inners = record.fields.iter().map(|field| {
            let field_name = format_ident!("{}", field.name);
            if let Some(_) = &field.bit_width {
                quote!{
                    #field_name : BitfieldWrap::new()
                }
            }
            else {
                let subtype = format_ident!("{}", field.subtype_name);
                quote! {
                    #field_name : #subtype::new()
                }
            }
        });


        Ok(quote! {
            #[derive(Debug, Clone, BitStructSerial)]
            pub struct #struct_name {
                #(#fields),*
            }

            impl #struct_name {
                pub fn new() -> Self {
                    #struct_name {
                        #(#constructor_inners),*
                    }
                }
            }
        })
    }).collect();

    let structs = structs?;

    let output = quote! {
        use bitfield_derive::BitStructSerial;
        use bitfield_struct::{BitStructSerial, BitfieldError, BitfieldWrap};

        #(#structs)*
    };

    let syntax_tree = syn::parse2(output).map_err(|e| VwError::CodeGen {
        message: format!("Failed to parse generated code: {}", e)
    })?;
    Ok(prettyplease::unparse(&syntax_tree))
}

fn process_sim_output(
    expr_keys : &Vec<String>,
    exprs_to_resolve : HashMap<String, Vec<ConstraintID>>,
    records : &mut Vec<ResolvedRecord>,
    sim_out : &Vec<u8>
) -> Result<(), VwError>{
    let stdout_str = String::from_utf8_lossy(sim_out);

    for line in stdout_str.lines() {
        let parts: Vec<&str> = line.splitn(2, ": ").collect();

        let index: usize = parts[0].strip_prefix("EXPR_").unwrap().parse().map_err(|e|
            VwError::CodeGen { message: 
                format!("Somehow generated an unparseable simulation output : {:}.\
                 Look at sim.out", e)
        })?;
        let value : usize = parts[1].parse().map_err(|e|
            VwError::CodeGen { message: 
            format!("Expression couldn't be evaluated : {}", e) 
        })?;

        let key = &expr_keys[index];
        let constraint_ids = exprs_to_resolve.get(key)
            .ok_or_else(||{
                VwError::CodeGen { message: 
                format!("Somehow got expression {:} which doesn't exist", key) 
            }})?;
        for id in constraint_ids {
            let record = &mut (records[id.record_index]);
            let field = &mut (record.fields[id.field_index]);
            if let Some(bitfield) = &mut field.bit_width {
                match id.side {
                    Side::Left => bitfield.left = Some(value),
                    Side::Right => bitfield.right = Some(value)
                }
            }
            else {
                return Err(VwError::CodeGen { message: 
                    format!("Somehow tried to generate an expression for \
                        field {:} in record {:} which has no expression",
                    field.name, record.name)
                })
            }
        }
    }
    Ok(())
}


fn create_testbench(
    exprs_to_resolve : &Vec<String>, 
    packages_needed : &Vec<String>,
) -> String {
    let mut testbench = Vec::new();

    testbench.push(generate_testbench_imports(packages_needed));

    testbench.push(String::from(
"entity constraint_evaluator is
end entity constraint_evaluator;

architecture behavior of constraint_evaluator is
begin
    process
        variable l : line;
    begin
        wait for 0ns;
"
    ));

    for (i, expr) in exprs_to_resolve.iter().enumerate() {
        testbench.push(format!("
        write(l, string'(\"EXPR_{}: \"));\n
        write(l, integer'image({}));\n
        writeline(OUTPUT, l);\n",
            i, expr
        ));
    }
    
    testbench.push(String::from(
        "        wait;
    end process;
end architecture behavior;
"
    ));

    testbench.join("")
}
    
/// Generate VHDL package use statements for a testbench
pub fn generate_testbench_imports(packages_needed : &Vec<String>) -> String {
    let mut imports = Vec::new();

    imports.push(String::from("-- Required packages\n"));
    imports.push(String::from("library ieee;\n"));
    imports.push(String::from("use ieee.std_logic_1164.all;\n"));
    imports.push(String::from("use ieee.numeric_std.all;\n"));
    imports.push(String::from("\n"));

    imports.push(String::from("library std;\n"));
    imports.push(String::from("use std.textio.all;\n"));

    for package_name in packages_needed {
        imports.push(format!("use work.{}.all;\n", package_name));
    }

    imports.join("")
}
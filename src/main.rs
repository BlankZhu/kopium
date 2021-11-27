#[macro_use] extern crate log;
use anyhow::{bail, Result};
use clap::{App, Arg};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
    CustomResourceDefinition, JSONSchemaProps, JSONSchemaPropsOrArray, JSONSchemaPropsOrBool,
};
use kube::{Api, Client};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new("kopium")
        .version(clap::crate_version!())
        .author("Eirik A <sszynrae@gmail.com>")
        .about("Kubernetes OPenapI UnMangler")
        .arg(
            Arg::new("crd")
                .about("Give the name of the input CRD to use e.g. prometheusrules.monitoring.coreos.com")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::new("v")
                .short('v')
                .multiple_occurrences(true)
                .takes_value(true)
                .about("Sets the level of verbosity"),
        )
        .get_matches();
    env_logger::init();

    let client = Client::try_default().await?;
    let api: Api<CustomResourceDefinition> = Api::all(client);
    let crd_name = matches.value_of("crd").unwrap();
    let crd = api.get(crd_name).await?;


    let mut data = None;
    let mut picked_version = None;

    // TODO: pick most suitable version or take arg for it
    let versions = crd.spec.versions;
    if let Some(v) = versions.first() {
        picked_version = Some(v.name.clone());
        if let Some(s) = &v.schema {
            if let Some(schema) = &s.open_api_v3_schema {
                data = Some(schema.clone())
            }
        }
    }
    let kind = crd.spec.names.kind;
    let group = crd.spec.group;
    let version = picked_version.expect("need one version in the crd");
    let scope = crd.spec.scope;


    if let Some(schema) = data {
        let mut results = vec![];
        debug!("schema: {}", serde_json::to_string_pretty(&schema)?);
        analyze(schema, &kind, "", 0, &mut results)?;

        print_prelude();
        for s in results {
            if s.level == 0 {
                continue; // ignoring root struct
            } else {
                if s.level == 1 && s.name.ends_with("Spec") {
                    println!("#[derive(CustomResource, Serialize, Deserialize, Clone, Debug)");
                    println!(
                        r#"#[kube(group = "{}", version = "{}", kind = "{}")"#,
                        group, version, kind
                    );
                    if scope == "Namespaced" {
                        println!(r#"#[kube(Namespaced)]"#);
                    }
                    // don't support grabbing original schema atm so disable schemas:
                    // (we coerce IntToString to String anyway so it wont match anyway)
                    println!(r#"#[kube(schema = "disabled")]"#);
                } else {
                    println!("#[derive(Serialize, Deserialize, Clone, Debug)");
                }
                println!("pub struct {} {{", s.name);
                for m in s.members {
                    if let Some(annot) = m.field_annot {
                        println!("    {}", annot);
                    }
                    println!("    pub {}: {},", m.name, m.type_);
                }
                println!("}}")
            }
        }
    } else {
        error!("no schema found for crd {}", crd_name);
    }

    Ok(())
}

fn print_prelude() {
    println!("use kube::CustomResource;");
    println!("use serde::{{Serialize, Deserialize}};");
    println!("use std::collections::BTreeMap;");
    println!();
}

#[derive(Default, Debug)]
struct OutputStruct {
    name: String,
    level: u8,
    members: Vec<OutputMember>,
}
#[derive(Default, Debug)]
struct OutputMember {
    name: String,
    type_: String,
    field_annot: Option<String>,
}

const IGNORED_KEYS: [&str; 3] = ["metadata", "apiVersion", "kind"];

// recursive entry point to analyze a schema and generate a struct for if object type
fn analyze(
    schema: JSONSchemaProps,
    kind: &str,
    root: &str,
    level: u8,
    results: &mut Vec<OutputStruct>,
) -> Result<()> {
    let props = schema.properties.unwrap_or_default();
    let mut array_recurse_level: HashMap<String, u8> = Default::default();
    // first generate the object if it is one
    let root_type = schema.type_.unwrap_or_default();
    if root_type == "object" {
        if let Some(additional) = schema.additional_properties {
            if let JSONSchemaPropsOrBool::Schema(s) = additional {
                let dict_type = s.type_.unwrap_or_default();
                if !dict_type.is_empty() {
                    warn!("not generating type {} - using map String->{}", root, dict_type);
                    return Ok(()); // no members here - it'll be inlined
                }
            }
        }
        let mut members = vec![];
        debug!("Generating struct {}{}", kind, root);

        let reqs = schema.required.unwrap_or_default();
        // initial analysis of properties (we do not recurse here, we need to find members first)
        for (key, value) in &props {
            let value_type = value.type_.clone().unwrap_or_default();
            let rust_type = match value_type.as_ref() {
                "object" => {
                    let mut dict_key = None;
                    if let Some(additional) = &value.additional_properties {
                        debug!("got additional: {}", serde_json::to_string(&additional)?);
                        if let JSONSchemaPropsOrBool::Schema(s) = additional {
                            let dict_type = s.type_.clone().unwrap_or_default();
                            dict_key = match dict_type.as_ref() {
                                "string" => Some("String".into()),
                                "" => {
                                    if s.x_kubernetes_int_or_string.is_some() {
                                        warn!("coercing presumed IntOrString {} to String", key);
                                        Some("String".into())
                                    } else {
                                        bail!("unknown empty dict type for {}", key)
                                    }
                                }
                                // think the type we get is the value type
                                x => Some(uppercase_first_letter(x)), // best guess
                            };
                        }
                    }
                    if let Some(dict) = dict_key {
                        format!("BTreeMap<String, {}>", dict)
                    } else {
                        let structsuffix = uppercase_first_letter(key);
                        // need to find the deterministic name for the struct
                        format!("{}{}", kind, structsuffix)
                    }
                }
                "string" => "String".to_string(),
                "boolean" => "bool".to_string(),
                "integer" => {
                    // need to look at the format here:
                    if let Some(f) = &value.format {
                        match f.as_ref() {
                            "int32" => "i32".to_string(),
                            "int64" => "i64".to_string(),
                            x => {
                                error!("unknown integer {}", x);
                                "usize".to_string()
                            }
                        }
                    } else {
                        "usize".to_string()
                    }
                }
                "array" => {
                    // recurse through repeated arrays until we find a concrete type (keep track of how deep we went)
                    let (array_type, recurse_level) = array_recurse_for_type(value, kind, key, 1)?;
                    debug!(
                        "got array type {} for {} in level {}",
                        array_type, key, recurse_level
                    );
                    array_recurse_level.insert(key.clone(), recurse_level);
                    array_type
                }
                "" => {
                    if value.x_kubernetes_int_or_string.is_some() {
                        warn!("coercing presumed IntOrString {} to String", key);
                        "String".into()
                    } else {
                        bail!("unknown empty dict type for {}", key)
                    }
                }
                x => bail!("unknown type {}", x),
            };

            // Create member and wrap types correctly
            if reqs.contains(key) {
                debug!("with required member {} of type {}", key, rust_type);
                members.push(OutputMember {
                    type_: rust_type,
                    name: key.to_string(),
                    field_annot: None,
                })
            } else {
                // option wrapping possibly needed if not required
                debug!("with optional member {} of type {}", key, rust_type);
                if rust_type.starts_with("BTreeMap") {
                    members.push(OutputMember {
                        type_: rust_type,
                        name: key.to_string(),
                        field_annot: Some(
                            r#"#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]"#.into(),
                        ),
                    })
                } else if rust_type.starts_with("Vec") {
                    members.push(OutputMember {
                        type_: rust_type,
                        name: key.to_string(),
                        field_annot: Some(
                            r#"#[serde(default, skip_serializing_if = "Vec::is_empty")]"#.into(),
                        ),
                    })
                } else {
                    members.push(OutputMember {
                        type_: format!("Option<{}>", rust_type),
                        name: key.to_string(),
                        field_annot: None,
                    })
                }
            }
        }
        // Finalize struct with given members
        results.push(OutputStruct {
            name: format!("{}{}", kind, root),
            members,
            level,
        });
    }

    // Start recursion for properties
    for (key, value) in props {
        if level == 0 && IGNORED_KEYS.contains(&(key.as_ref())) {
            debug!("not recursing into ignored {}", key); // handled elsewhere
            continue;
        }
        let value_type = value.type_.clone().unwrap_or_default();
        match value_type.as_ref() {
            "object" => {
                // recurse
                let structsuffix = uppercase_first_letter(&key);
                analyze(value, kind, &structsuffix, level + 1, results)?;
            }
            "array" => {
                if let Some(recurse) = array_recurse_level.get(&key).cloned() {
                    let structsuffix = uppercase_first_letter(&key);
                    let mut inner = value.clone();
                    for _i in 0..recurse {
                        debug!("recursing into props for {}", key);
                        if let Some(sub) = inner.items {
                            match sub {
                                JSONSchemaPropsOrArray::Schema(s) => {
                                    //info!("got inner: {}", serde_json::to_string_pretty(&s)?);
                                    inner = *s.clone();
                                }
                                _ => bail!("only handling single type in arrays"),
                            }
                        } else {
                            bail!("could not recurse into vec");
                        }
                    }

                    analyze(inner, kind, &structsuffix, level + 1, results)?;
                }
            }
            "" => {
                if value.x_kubernetes_int_or_string.is_some() {
                    debug!("not recursing into IntOrString {}", key)
                } else {
                    debug!("not recursing into unknown empty type {}", key)
                }
            }
            x => debug!("not recursing into {} (not a container - {})", key, x),
        }
    }
    Ok(())
}


fn uppercase_first_letter(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

fn array_recurse_for_type(value: &JSONSchemaProps, kind: &str, key: &str, level: u8) -> Result<(String, u8)> {
    if let Some(items) = &value.items {
        match items {
            JSONSchemaPropsOrArray::Schema(s) => {
                let inner_array_type = s.type_.clone().unwrap_or_default();
                return match inner_array_type.as_ref() {
                    "object" => {
                        let structsuffix = uppercase_first_letter(key);
                        Ok((format!("Vec<{}{}>", kind, structsuffix), level))
                    }
                    "string" => Ok(("Vec<String>".into(), level)),
                    "boolean" => Ok(("Vec<bool>".into(), level)),
                    "integer" => {
                        // need to look at the format here:
                        let int_type = if let Some(f) = &s.format {
                            match f.as_ref() {
                                "int32" => "i32".to_string(),
                                "int64" => "i64".to_string(),
                                x => {
                                    error!("unknown integer {}", x);
                                    "usize".to_string()
                                }
                            }
                        } else {
                            "usize".to_string()
                        };
                        Ok((format!("Vec<{}>", int_type), level))
                    }
                    "array" => Ok(array_recurse_for_type(s, kind, key, level + 1)?),
                    x => {
                        bail!("unsupported recursive array type {} for {}", x, key)
                    }
                };
            }
            // maybe fallback to serde_json::Value
            _ => bail!("only support single schema in array {}", key),
        }
    } else {
        bail!("missing items in array type")
    }
}

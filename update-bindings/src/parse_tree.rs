use convert_case::{Case, Casing};
use quote::{quote, ToTokens};
use regex::Regex;
use std::{
    collections::BTreeMap,
    fmt::{self, Debug, Display, Formatter},
    fs,
    io::Write,
    iter::{self, Iterator},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

pub fn generate_bindings(source_path: &Path) -> crate::Result<PathBuf> {
    let bindings = crate::read_bindings(source_path)?;
    let parsed = syn::parse_file(&bindings)?;
    let parse_tree = ParseTree::try_from(&parsed)?;

    let mut out_file = crate::dirs::get_out_dir();
    out_file.push("bindings.rs");
    let mut bindings = fs::File::create(&out_file)?;
    write!(bindings, "{}", parse_tree)?;
    format_bindings(&out_file)?;

    Ok(out_file)
}

#[derive(Debug, Error)]
pub enum Unrecognized {
    #[error("Unrecognized Field Type")]
    FieldType,
    #[error("Unrecognized Function Argument")]
    FnArg,
    #[error("Unrecognized Generic Type")]
    Generic,
    #[error("Unrecognized Interface Declaration")]
    Interface,
    #[error("Failed to Parse Bindings")]
    Parse(#[from] syn::Error),
}

#[derive(Debug)]
struct MethodArgument {
    name: String,
    rust_name: String,
    ty: String,
    cef_type: String,
}

#[derive(Debug)]
struct MethodDeclaration {
    name: String,
    original_name: Option<String>,
    args: Vec<MethodArgument>,
    output: Option<String>,
    original_output: Option<String>,
}

impl Display for MethodDeclaration {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let name = &self.name;
        let args = self
            .args
            .iter()
            .map(|arg| {
                if arg.name == "self_" {
                    String::from("&self")
                } else {
                    format!("{}: {}", arg.rust_name, arg.ty)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let output = self
            .output
            .as_deref()
            .map(|output| format!(" -> {output}"))
            .unwrap_or_default();
        write!(f, "fn {name}({args}){output}")
    }
}

impl TryFrom<&syn::Field> for MethodDeclaration {
    type Error = Unrecognized;

    fn try_from(value: &syn::Field) -> Result<Self, Self::Error> {
        let name = value
            .ident
            .as_ref()
            .ok_or(Unrecognized::FieldType)?
            .to_string();

        // Look for a type matching std::option::Option<T>
        let syn::Type::Path(syn::TypePath {
            qself: None,
            path: syn::Path { segments, .. },
        }) = &value.ty
        else {
            return Err(Unrecognized::FieldType);
        };
        let mut segments_iter = segments.iter();
        let (
            Some(syn::PathSegment {
                ident: ident_std,
                arguments: syn::PathArguments::None,
            }),
            Some(syn::PathSegment {
                ident: ident_option,
                arguments: syn::PathArguments::None,
            }),
            Some(syn::PathSegment {
                ident: ident_type,
                arguments:
                    syn::PathArguments::AngleBracketed(syn::AngleBracketedGenericArguments {
                        args, ..
                    }),
            }),
            None,
        ) = (
            segments_iter.next(),
            segments_iter.next(),
            segments_iter.next(),
            segments_iter.next(),
        )
        else {
            return Err(Unrecognized::FieldType);
        };
        if ident_std.to_string() != "std"
            || ident_option.to_string() != "option"
            || ident_type.to_string() != "Option"
            || args.len() != 1
        {
            return Err(Unrecognized::FieldType);
        }

        // See if the Option<T> type is a function pointer
        let mut args = args.iter();
        let (
            Some(syn::GenericArgument::Type(syn::Type::BareFn(syn::TypeBareFn {
                unsafety: Some(_),
                abi: Some(syn::Abi {
                    name: Some(abi), ..
                }),
                inputs,
                variadic: None,
                output,
                ..
            }))),
            None,
        ) = (args.next(), args.next())
        else {
            return Err(Unrecognized::FieldType);
        };
        if abi.value() != "C" {
            return Err(Unrecognized::FieldType);
        }

        // Looks like a match, convert it to a MethodDeclaration
        let args = inputs
            .iter()
            .filter_map(|arg| {
                if let syn::BareFnArg {
                    name: Some((name, _)),
                    ty,
                    ..
                } = arg
                {
                    let name = name.to_string();
                    let rust_name = make_snake_case_value_name(&name);
                    let cef_type = ty.to_token_stream().to_string();
                    let ty = type_to_string(ty);
                    Some(MethodArgument {
                        name,
                        rust_name,
                        ty,
                        cef_type,
                    })
                } else {
                    None
                }
            })
            .collect();
        let (original_output, output) = match output {
            syn::ReturnType::Type(_, ty) => (
                Some(ty.to_token_stream().to_string()),
                Some(type_to_string(ty)),
            ),
            _ => (None, None),
        };

        Ok(Self {
            name,
            original_name: None,
            args,
            output,
            original_output,
        })
    }
}

#[derive(Debug)]
struct FieldDeclaration {
    name: String,
    rust_name: String,
    ty: String,
}

impl Display for FieldDeclaration {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let rust_name = &self.rust_name;
        let ty = &self.ty;
        write!(f, "pub {rust_name}: {ty},")
    }
}

impl TryFrom<&syn::Field> for FieldDeclaration {
    type Error = Unrecognized;

    fn try_from(value: &syn::Field) -> Result<Self, Self::Error> {
        let name = value
            .ident
            .as_ref()
            .ok_or(Unrecognized::FieldType)?
            .to_string();
        let rust_name = make_snake_case_value_name(&name);
        let ty = type_to_string(&value.ty);

        Ok(Self {
            name,
            rust_name,
            ty,
        })
    }
}

#[derive(Debug, Default)]
struct StructDeclaration {
    name: String,
    rust_name: Option<String>,
    fields: Vec<FieldDeclaration>,
    methods: Vec<MethodDeclaration>,
}

#[derive(Debug, Default)]
struct BaseTypes(BTreeMap<String, String>);

impl BaseTypes {
    fn new<'a>(structs: impl Iterator<Item = &'a StructDeclaration>) -> Self {
        Self(
            structs
                .filter_map(|s| {
                    if s.fields.iter().map(|f| f.name.as_str()).eq(["base"]) {
                        s.fields
                            .get(0)
                            .and_then(|f| s.rust_name.as_ref().map(|n| (n.clone(), f.ty.clone())))
                    } else {
                        None
                    }
                })
                .collect(),
        )
    }

    fn base(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    fn root<'a: 'b, 'b>(&'a self, name: &'b str) -> &'b str {
        self.base(name).map(|base| self.root(base)).unwrap_or(name)
    }
}

#[derive(Debug, Default)]
struct ParseTree {
    type_aliases: BTreeMap<String, String>,
    enums: Vec<String>,
    structs: Vec<StructDeclaration>,
    base_types: BaseTypes,
    globals: Vec<MethodDeclaration>,
}

impl ParseTree {
    pub fn write_prelude(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let header = quote! {
            #![allow(dead_code, non_camel_case_types, unused_variables)]
            use crate::{
                rc::{RcImpl, RefGuard},
                wrapper,
            };
            use cef_sys::*;
        }
        .to_string();
        writeln!(f, "{}", header)
    }

    pub fn write_aliases(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "\n// Type aliases")?;
        for (alias_name, alias_ty) in &self.type_aliases {
            let alias_name = make_rust_type_name(&alias_name).unwrap_or_else(|| alias_name.clone());
            let alias_ty = make_rust_type_name(&alias_ty).unwrap_or_else(|| alias_ty.clone());
            if alias_name != alias_ty {
                writeln!(f, "pub type {} = {};", alias_name, alias_ty)?;
            }
        }
        Ok(())
    }

    pub fn write_structs(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "\n// Struct wrappers")?;
        for StructDeclaration {
            name,
            rust_name,
            fields,
            methods,
        } in &self.structs
        {
            let Some(rust_name) = rust_name.as_ref() else {
                continue;
            };

            let root = self.base_types.root(rust_name);
            if root == "BaseRefCounted" && root != rust_name {
                write!(
                    f,
                    r#"
                        wrapper!(
                            #[doc = "See [{name}] for more documentation."]
                            #[derive(Clone)]
                            pub struct {rust_name}({name});
                    "#
                )?;
                for method in methods {
                    write!(f, "\n    pub {method};")?;
                }

                let base_rust_name = self.base_types.base(rust_name);
                let base_trait = base_rust_name
                    .and_then(|base| {
                        if base == root {
                            Some(String::from(": Sized"))
                        } else {
                            Some(format!(": Impl{base}"))
                        }
                    })
                    .unwrap_or_default();
                write!(
                    f,
                    r#"
                        );

                        pub trait Impl{rust_name}{base_trait} {{
                    "#
                )?;

                for method in methods {
                    let output = method
                        .output
                        .as_deref()
                        .map(|_| String::from(" Default::default() "))
                        .unwrap_or_default();
                    writeln!(f, "    {method} {{{output}}}")?;
                }

                let mut base_rust_name = base_rust_name;
                let mut base_structs = vec![];
                while let Some(next_base) =
                    base_rust_name
                        .filter(|base| *base != root)
                        .and_then(|base| {
                            self.structs
                                .iter()
                                .find(|s| s.rust_name.as_deref() == Some(base))
                        })
                {
                    base_rust_name = next_base
                        .rust_name
                        .as_ref()
                        .and_then(|name| self.base_types.base(name.as_str()));
                    base_structs.push(next_base);
                }

                let init_bases = base_structs
                    .into_iter()
                    .enumerate()
                    .map(|(i, base_struct)| {
                        let name = &base_struct.name;
                        let bases = iter::repeat_n("base", i + 1).collect::<Vec<_>>().join(".");
                        format!(r#"impl{name}::init_methods::<Self>(&mut object.{bases});"#)
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev();

                write!(
                    f,
                    r#"
                            fn into_raw(self) -> *mut {name} {{
                                let mut object: {name} = unsafe {{ std::mem::zeroed() }};"#
                )?;

                for init_base in init_bases {
                    write!(f, "\n{init_base}")?;
                }

                write!(
                    f,
                    r#"
                                impl{name}::init_methods::<Self>(&mut object);
                                RcImpl::new(object, self) as *mut _
                            }}
                        }}

                        mod impl{name} {{
                            use super::*;

                            pub fn init_methods<I: Impl{rust_name}>(object: &mut {name}) {{"#
                )?;

                for method in methods {
                    let name = &method.name;
                    write!(f, r#"object.{name} = Some({name}::<I>);"#)?;
                }

                writeln!(
                    f,
                    r#"
                            }}
                    "#
                )?;

                for method in methods {
                    let name = &method.name;
                    let args = method
                        .args
                        .iter()
                        .map(|arg| format!("{}: {}", arg.rust_name, arg.cef_type))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let forward_args = method
                        .args
                        .iter()
                        .skip(1)
                        .map(|arg| format!("{}.into()", arg.rust_name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let output = method
                        .original_output
                        .as_deref()
                        .map(|output| format!(" -> {output}"))
                        .unwrap_or_default();
                    let forward_output = method
                        .original_output
                        .as_deref()
                        .map(|_| String::from(".into()"))
                        .unwrap_or_default();
                    writeln!(
                        f,
                        r#"
                            extern "C" fn {name}<I: Impl{rust_name}>({args}){output} {{
                                let obj: &RcImpl<_, I> = RcImpl::get(self_);
                                obj.interface.{name}({forward_args}){forward_output}
                            }}
                        "#
                    )?;
                }

                writeln!(f, r#"}}"#)?;
            } else if !methods.is_empty()
                || fields.is_empty()
                || fields.iter().map(|f| f.name.as_str()).eq(["_unused"])
            {
                write!(
                    f,
                    r#"
                        /// See [{name}] for more documentation.
                        pub struct {rust_name}({name});

                        impl From<{name}> for {rust_name} {{
                            fn from(value: {name}) -> Self {{
                                Self(value)
                            }}
                        }}

                        impl Into<{name}> for {rust_name} {{
                            fn into(self) -> {name} {{
                                self.0
                            }}
                        }}

                        impl AsRef<{name}> for {rust_name} {{
                            fn as_ref(&self) -> &{name} {{
                                &self.0
                            }}
                        }}

                        impl AsMut<{name}> for {rust_name} {{
                            fn as_mut(&mut self) -> &mut {name} {{
                                &mut self.0
                            }}
                        }}

                        impl Default for {rust_name} {{
                            fn default() -> Self {{
                                unsafe {{ std::mem::zeroed() }}
                            }}
                        }}
                    "#
                )?;
            } else {
                writeln!(f, "\n/// See [{name}] for more documentation.")?;
                writeln!(f, "pub struct {rust_name} {{")?;
                for field in fields {
                    writeln!(f, "    {field}")?;
                }
                writeln!(f, "}}")?;
                write!(
                    f,
                    r#"
                        impl From<{name}> for {rust_name} {{
                            fn from(value: {name}) -> Self {{
                                Self {{"#
                )?;

                for field in fields {
                    let name = &field.name;
                    let rust_name = &field.rust_name;
                    write!(f, "\n{rust_name}: value.{name}.into(),")?;
                }

                write!(
                    f,
                    r#"
                                }}
                            }}
                        }}

                        impl Into<{name}> for {rust_name} {{
                            fn into(self) -> {name} {{
                                {name} {{"#
                )?;

                for field in fields {
                    let name = &field.name;
                    let rust_name = &field.rust_name;
                    write!(f, "\n{name}: self.{rust_name}.into(),")?;
                }

                write!(
                    f,
                    r#"
                                }}
                            }}
                        }}

                        impl Default for {rust_name} {{
                            fn default() -> Self {{
                                unsafe {{ std::mem::zeroed() }}
                            }}
                        }}
                    "#
                )?;
            }
        }
        Ok(())
    }

    pub fn write_enums(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "\n// Enum aliases")?;
        for name in &self.enums {
            let Some(rust_name) = make_rust_type_name(name) else {
                continue;
            };
            write!(
                f,
                r#"
                    /// See [{name}] for more documentation.
                    #[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
                    pub struct {rust_name}({name});

                    impl AsRef<{name}> for {rust_name} {{
                        fn as_ref(&self) -> &{name} {{
                            &self.0
                        }}
                    }}

                    impl AsMut<{name}> for {rust_name} {{
                        fn as_mut(&mut self) -> &mut {name} {{
                            &mut self.0
                        }}
                    }}

                    impl From<{name}> for {rust_name} {{
                        fn from(value: {name}) -> Self {{
                            Self(value)
                        }}
                    }}

                    impl Into<{name}> for {rust_name} {{
                        fn into(self) -> {name} {{
                            self.0
                        }}
                    }}

                    impl Default for {rust_name} {{
                        fn default() -> Self {{
                            unsafe {{ std::mem::zeroed() }}
                        }}
                    }}
                "#
            )?;
        }
        Ok(())
    }

    pub fn write_globals(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "\n// Global function wrappers")?;
        for global_fn in &self.globals {
            let original_name = global_fn.original_name.as_ref().unwrap_or(&global_fn.name);
            let args = global_fn
                .args
                .iter()
                .map(|arg| format!("{}.into()", arg.rust_name))
                .collect::<Vec<_>>()
                .join(", ");
            let output = global_fn
                .output
                .as_deref()
                .map(|_| String::from(".into()"))
                .unwrap_or_default();
            writeln!(
                f,
                r#"
                    pub {global_fn} {{
                        unsafe {{ {original_name}({args}){output} }}
                    }}
                "#
            )?;
        }
        Ok(())
    }
}

impl Display for ParseTree {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.write_prelude(f)?;
        self.write_aliases(f)?;
        self.write_structs(f)?;
        self.write_enums(f)?;
        self.write_globals(f)
    }
}

impl TryFrom<&syn::File> for ParseTree {
    type Error = Unrecognized;

    fn try_from(value: &syn::File) -> Result<Self, Self::Error> {
        let mut tree = Self::default();
        for item in &value.items {
            match item {
                syn::Item::Type(item_type) => {
                    let alias_name = item_type.ident.to_string();
                    let alias_ty = type_to_string(&item_type.ty);
                    tree.type_aliases.insert(alias_name, alias_ty);
                }
                syn::Item::Struct(item_struct) => match &item_struct.fields {
                    syn::Fields::Named(fields) => {
                        let mut struct_decl = StructDeclaration::default();
                        struct_decl.name = item_struct.ident.to_string();
                        struct_decl.rust_name = make_rust_type_name(&struct_decl.name);
                        for field in fields.named.iter() {
                            if let Ok(field_decl) = MethodDeclaration::try_from(field) {
                                struct_decl.methods.push(field_decl);
                            } else if let Ok(field_decl) = FieldDeclaration::try_from(field) {
                                struct_decl.fields.push(field_decl);
                            }
                        }
                        tree.structs.push(struct_decl);
                    }
                    syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                        tree.enums.push(item_struct.ident.to_string());
                    }
                    _ => {}
                },
                syn::Item::Enum(syn::ItemEnum { ident, .. }) => {
                    tree.enums.push(ident.to_string());
                }
                syn::Item::ForeignMod(syn::ItemForeignMod {
                    unsafety: Some(_),
                    abi:
                        syn::Abi {
                            name: Some(abi), ..
                        },
                    items,
                    ..
                }) if abi.value() == "C" => {
                    for item in items {
                        if let syn::ForeignItem::Fn(item_fn) = item {
                            let original_name = item_fn.sig.ident.to_string();
                            static PATTERN: OnceLock<Regex> = OnceLock::new();
                            let pattern =
                                PATTERN.get_or_init(|| Regex::new(r"^cef_(\w+)$").unwrap());
                            let name = pattern
                                .captures(&original_name)
                                .and_then(|captures| captures.get(1))
                                .map(|name| name.as_str().to_string());
                            let (name, original_name) = match name {
                                Some(name) => (name, Some(original_name)),
                                None => (original_name, None),
                            };
                            let args = item_fn
                                .sig
                                .inputs
                                .iter()
                                .filter_map(|arg| {
                                    let syn::FnArg::Typed(syn::PatType { pat, ty, .. }) = arg
                                    else {
                                        return None;
                                    };

                                    let syn::Pat::Ident(syn::PatIdent { ident, .. }) = pat.as_ref()
                                    else {
                                        return None;
                                    };

                                    let name = ident.to_string();
                                    let rust_name = make_snake_case_value_name(&name);
                                    let cef_type = ty.to_token_stream().to_string();
                                    let ty = type_to_string(ty.as_ref());
                                    Some(MethodArgument {
                                        name,
                                        rust_name,
                                        ty,
                                        cef_type,
                                    })
                                })
                                .collect();
                            let (original_output, output) = match &item_fn.sig.output {
                                syn::ReturnType::Type(_, ty) => (
                                    Some(ty.to_token_stream().to_string()),
                                    Some(type_to_string(ty.as_ref())),
                                ),
                                _ => (None, None),
                            };
                            tree.globals.push(MethodDeclaration {
                                name,
                                original_name,
                                args,
                                output,
                                original_output,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        tree.base_types = BaseTypes::new(tree.structs.iter());

        Ok(tree)
    }
}

fn format_bindings(source_path: &Path) -> crate::Result<()> {
    let mut cmd = Command::new("rustfmt");
    cmd.arg(source_path);
    cmd.output()?;
    Ok(())
}

fn type_to_string(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(syn::TypePath { qself: None, path }) => {
            let name = path.to_token_stream().to_string();
            make_rust_type_name(&name).unwrap_or(name)
        }
        syn::Type::Tuple(syn::TypeTuple { elems, .. }) => {
            let elems = elems
                .iter()
                .map(|elem| type_to_string(elem))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({elems})")
        }
        syn::Type::Array(syn::TypeArray { elem, len, .. }) => {
            let elem = type_to_string(elem);
            let len = len.to_token_stream().to_string();
            format!("[{elem}; {len}]")
        }
        syn::Type::Slice(syn::TypeSlice { elem, .. }) => {
            let elem = type_to_string(elem);
            format!("[{elem}]")
        }
        syn::Type::Ptr(syn::TypePtr {
            const_token, elem, ..
        }) => {
            let rust_name = match elem.as_ref() {
                syn::Type::Path(syn::TypePath { qself: None, path }) => {
                    let name = path.to_token_stream().to_string();
                    make_rust_type_name(&name)
                }
                _ => None,
            };

            match (rust_name, const_token) {
                (Some(rust_name), _) => rust_name,
                (None, Some(_)) => format!("*const {}", type_to_string(elem.as_ref())),
                (None, None) => format!("*mut {}", type_to_string(elem.as_ref())),
            }
        }
        _ => ty.to_token_stream().to_string(),
    }
}

fn make_rust_type_name(name: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| Regex::new(r"^_?cef_(\w+)_t$").unwrap());
    pattern
        .captures(name)
        .and_then(|captures| captures.get(1))
        .map(|name| {
            let name = name
                .as_str()
                .from_case(Case::Snake)
                .to_case(Case::UpperCamel);
            if name.starts_with("String") {
                format!("Cef{}", name)
            } else {
                name
            }
        })
}

fn make_snake_case_value_name(name: &str) -> String {
    name.from_case(Case::Camel).to_case(Case::Snake)
}

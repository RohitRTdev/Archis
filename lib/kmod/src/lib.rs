use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, Ident, ItemFn, LitBool, Token, parse_macro_input, punctuated::Punctuated};
use syn::parse::{Parse, ParseStream};

#[proc_macro_attribute]
pub fn init(
    attr: TokenStream,
    item: TokenStream,
) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);

    let is_driver = if attr.is_empty() {
        false
    } else {
        let ident = parse_macro_input!(attr as syn::Ident);

        if ident != "driver" {
            return syn::Error::new_spanned(
                ident,
                "expected `driver`",
            )
            .to_compile_error()
            .into();
        }

        true
    };

    let ident = &func.sig.ident;

    let entry = if is_driver {
        quote! {
            mod import_stub;
            use import_stub::*;

            #[unsafe(no_mangle)]
            extern "C" fn _shim_module_init() {}

            #[unsafe(no_mangle)]
            extern "C" fn _shim_driver_init(
                driver: *mut kernel_intf::driver::DriverObject
            ) -> kernel_intf::driver::Status {
                let obj = unsafe { &mut *driver };
                let result = #ident(obj);
                
                result
            }
        }
    } else {
        quote! {
            #[unsafe(no_mangle)]
            extern "C" fn _shim_module_init() {
                kernel_intf::run_tests!();
                #ident();
            }
        }
    };

    quote! {
        extern crate alloc;

        static MODULE_NAME_STR: &'static str = env!("CARGO_PKG_NAME");

        #[cfg(not(test))]
        #[panic_handler]
        fn panic(
            info: &core::panic::PanicInfo
        ) -> ! {
            let message = info
                .message()
                .as_str()
                .or(Some("Panicking!"))
                .unwrap();

            let mod_name =
                common::StrRef::from_str(MODULE_NAME_STR);

            let message_ref =
                common::StrRef::from_str(message);

            unsafe {
                kernel_intf::panic_router(
                    mod_name,
                    message_ref,
                )
            }
        }

        #[unsafe(no_mangle)]
        extern "C" fn _shim_module_config()
            -> common::StrRef
        {
            kernel_intf::init_logger(
                MODULE_NAME_STR
            );

            kernel_intf::enable_timestamp();

            common::StrRef::from_str(
                MODULE_NAME_STR
            )
        }

        #func

        #entry
    }
    .into()
}

#[proc_macro_attribute]
pub fn driver_unload(
    _attr: TokenStream,
    item: TokenStream,
) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);

    let ident = &func.sig.ident;

    quote! {
        #func
        #[unsafe(no_mangle)]
        extern "C" fn _shim_driver_unload(
            driver: *mut kernel_intf::driver::DriverObject
        ) {
            let obj = unsafe { &mut *driver };
            #ident(obj)
        }
    }
    .into()
}



#[proc_macro_attribute]
pub fn export(
    _attr: TokenStream,
    item: TokenStream,
) -> TokenStream {
    let mut func = parse_macro_input!(item as ItemFn);

    func.sig.abi = Some(syn::parse_quote!(
        extern "C"
    ));

    let attrs = &func.attrs;
    let vis = &func.vis;
    let sig = &func.sig;
    let block = &func.block;

    quote! {
        #[unsafe(no_mangle)]
        #(#attrs)*
        #vis
        #sig
        #block
    }
    .into()
}

#[proc_macro_attribute]
pub fn dispatch_handler(
    _attr: TokenStream,
    item: TokenStream
) -> TokenStream {
    const DEVICE_HANDLERS: [&str; 6] = [
        "dispatch_read",
        "dispatch_write",
        "dispatch_pnp",
        "dispatch_control",
        "dispatch_open",
        "dispatch_close",
    ];

    let func = parse_macro_input!(item as ItemFn);
    let ident = &func.sig.ident;
    let name = ident.to_string();
    let shim_ident = format_ident!("_shim_{}", ident);

    if name == "dispatch_add" {
        return quote! {
            #func

            unsafe extern "C" fn #shim_ident(
                driver: *const kernel_intf::driver::DriverObject,
                pdo: *const kernel_intf::driver::DeviceObject
            ) -> kernel_intf::driver::Status {
                let driver = unsafe { &*driver };
                let pdo = if pdo.is_null() {
                    None
                } else {
                    Some(unsafe { &*pdo })
                };

                #ident(driver, pdo)
            }
        }.into();
    }

    if !DEVICE_HANDLERS.contains(&name.as_str()) {
        return syn::Error::new_spanned(
            &func.sig.ident,
            "Handler name must be one of: dispatch_add, dispatch_read, dispatch_write, dispatch_pnp, dispatch_control, dispatch_open, dispatch_close"
        )
        .to_compile_error()
        .into();
    }

    quote! {
        #func

        unsafe extern "C" fn #shim_ident(
            device: *const kernel_intf::driver::DeviceObject,
            req: *mut kernel_intf::driver::Irp
        ) -> kernel_intf::driver::Status {
            let dev = unsafe { &*device };
            let irp = unsafe { &mut *req };

            #ident(dev, irp)
        }
    }.into()
}

struct DispatchInit {
    obj: Expr,
    handlers: Punctuated<Ident, Token![,]>,
}

impl Parse for DispatchInit {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let obj = input.parse()?;

        input.parse::<Token![,]>()?;

        let handlers =
            Punctuated::<Ident, Token![,]>::parse_terminated(input)?;

        Ok(Self {
            obj,
            handlers,
        })
    }
}

#[proc_macro_attribute]
pub fn test_function(attr: TokenStream, item: TokenStream) -> TokenStream {
    let enabled = parse_macro_input!(attr as LitBool);
    let func = parse_macro_input!(item as ItemFn);

    if !func.sig.inputs.is_empty() {
        return syn::Error::new_spanned(
            &func.sig.inputs,
            "test_function: function must take no arguments"
        )
        .to_compile_error()
        .into();
    }

    match &func.sig.output {
        syn::ReturnType::Default => {}
        syn::ReturnType::Type(_, ty) => {
            if !matches!(ty.as_ref(), syn::Type::Tuple(t) if t.elems.is_empty()) {
                return syn::Error::new_spanned(
                    &func.sig.output,
                    "test_function: function must return ()"
                )
                .to_compile_error()
                .into();
            }
        }
    }

    let ident = &func.sig.ident;
    let name_str = ident.to_string();
    let name_bytes = format!("{}\0", name_str);
    let name_len = name_str.len();
    let enabled_val = enabled.value;

    let shim_ident = format_ident!("_kunit_shim_{}", ident);
    let entry_ident = format_ident!("_KUNIT_ENTRY_{}", &name_str.to_uppercase());

    quote! {
        #[cfg(feature = "kunit-test")]
        #func

        #[cfg(feature = "kunit-test")]
        unsafe extern "C" fn #shim_ident() { #ident() }

        #[cfg(feature = "kunit-test")]
        #[used]
        #[unsafe(link_section = ".kunit_tests")]
        static #entry_ident: kernel_intf::kunit::KUnitEntry =
            kernel_intf::kunit::KUnitEntry {
                name: #name_bytes.as_ptr(),
                name_len: #name_len,
                func: #shim_ident,
                enabled: #enabled_val
            };
    }
    .into()
}

#[proc_macro]
pub fn dispatch_init(input: TokenStream) -> TokenStream {
    let DispatchInit {
        obj,
        handlers,
    } = parse_macro_input!(input as DispatchInit);

    let assignments = handlers.iter().map(|handler| {
        let shim = format_ident!("_shim_{}", handler);

        quote! {
            #obj.dispatch.#handler = Some(#shim);
        }
    });

    quote! {
        #(
            #assignments
        )*
    }
    .into()
}


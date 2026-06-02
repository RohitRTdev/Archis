use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, Ident, ItemFn, Token, parse_macro_input, punctuated::Punctuated};
use syn::parse::{Parse, ParseStream}; 

#[proc_macro_attribute]
pub fn init(
    _attr: TokenStream,
    item: TokenStream,
) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);

    if func.sig.ident != "driver_init" {
        return syn::Error::new_spanned(
            &func.sig.ident,
            "Expected module init fn: driver_init",
        )
        .to_compile_error()
        .into(); 
    }

    let ident = &func.sig.ident;

    quote! {
        extern crate alloc;
        static MODULE_NAME_STR: &'static str = env!("CARGO_PKG_NAME");

        #[cfg(not(test))]
        #[panic_handler]
        fn panic(info: &core::panic::PanicInfo) -> ! {
            let message = info.message().as_str().or(Some("Panicking!")).unwrap();
            let mod_name = common::StrRef::from_str(MODULE_NAME_STR);
            let message_ref = common::StrRef::from_str(message);
            unsafe {kernel_intf::panic_router(mod_name, message_ref)}
        }

        #[unsafe(no_mangle)]
        extern "C" fn module_config() -> common::StrRef {
            kernel_intf::init_logger(
                MODULE_NAME_STR
            );

            kernel_intf::enable_timestamp();

            common::StrRef::from_str(
                MODULE_NAME_STR
            )
        }

        mod import_stub;
        use import_stub::*;

        #func

        #[unsafe(no_mangle)]
        extern "C" fn shim_driver_init(driver: *mut kernel_intf::driver::DriverObject) -> kernel_intf::driver::Status {
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
pub fn handler(
    _attr: TokenStream,
    item: TokenStream
) -> TokenStream {
    const ALLOWED_FUNCTIONS: [&str; 2] = ["dispatch_read", "dispatch_write"];
    let func = parse_macro_input!(item as ItemFn);

    let ident = &func.sig.ident;
    if !ALLOWED_FUNCTIONS.iter().any(|s|  {
        *s == ident.to_string()
    }) {
        return syn::Error::new_spanned(
            &func.sig.ident,
            "Handler name must one of predefined dispatch_* names",
        )
        .to_compile_error()
        .into(); 
    }

    let shim_ident = format_ident!("shim_{}", ident);

    quote! {
        #func

        #[unsafe(no_mangle)]
        unsafe extern "C" fn #shim_ident(device: *const kernel_intf::driver::DeviceObject,
            req: *const kernel_intf::driver::Irp) -> Status {
            
            let dev = unsafe { &*device };
            let irp = unsafe { &*req };

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

#[proc_macro]
pub fn dispatch_init(input: TokenStream) -> TokenStream {
    let DispatchInit {
        obj,
        handlers,
    } = parse_macro_input!(input as DispatchInit);

    let assignments = handlers.iter().map(|handler| {
        let shim = format_ident!("shim_{}", handler);

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

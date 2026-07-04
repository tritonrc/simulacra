use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::{Function, Object};

/// Native module definition for `simulacra:fs`.
///
/// Exports: `readFile`, `writeFile`, `existsSync`, `mkdirSync`,
/// `readdirSync`, `statSync`, `unlinkSync`, `renameSync`, `appendFileSync`, `default`.
/// Delegates to the global `fs` object's host functions.
pub(crate) struct FsModule;

impl ModuleDef for FsModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("readFile")?;
        decl.declare("writeFile")?;
        decl.declare("existsSync")?;
        decl.declare("mkdirSync")?;
        decl.declare("readdirSync")?;
        decl.declare("statSync")?;
        decl.declare("unlinkSync")?;
        decl.declare("renameSync")?;
        decl.declare("appendFileSync")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &rquickjs::Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let fs_global: Object<'js> = globals.get("fs")?;

        let read_fn: Function<'js> = fs_global.get("readFileSync")?;
        exports.export("readFile", read_fn.clone())?;

        let write_fn: Function<'js> = fs_global.get("writeFileSync")?;
        exports.export("writeFile", write_fn.clone())?;

        let exists_fn: Function<'js> = fs_global.get("existsSync")?;
        exports.export("existsSync", exists_fn.clone())?;

        let mkdir_fn: Function<'js> = fs_global.get("mkdirSync")?;
        exports.export("mkdirSync", mkdir_fn.clone())?;

        let readdir_fn: Function<'js> = fs_global.get("readdirSync")?;
        exports.export("readdirSync", readdir_fn.clone())?;

        let stat_fn: Function<'js> = fs_global.get("statSync")?;
        exports.export("statSync", stat_fn.clone())?;

        let unlink_fn: Function<'js> = fs_global.get("unlinkSync")?;
        exports.export("unlinkSync", unlink_fn.clone())?;

        let rename_fn: Function<'js> = fs_global.get("renameSync")?;
        exports.export("renameSync", rename_fn.clone())?;

        let append_fn: Function<'js> = fs_global.get("appendFileSync")?;
        exports.export("appendFileSync", append_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("readFile", read_fn)?;
        default_obj.set("writeFile", write_fn)?;
        default_obj.set("existsSync", exists_fn)?;
        default_obj.set("mkdirSync", mkdir_fn)?;
        default_obj.set("readdirSync", readdir_fn)?;
        default_obj.set("statSync", stat_fn)?;
        default_obj.set("unlinkSync", unlink_fn)?;
        default_obj.set("renameSync", rename_fn)?;
        default_obj.set("appendFileSync", append_fn)?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

/// Native module definition for `simulacra:console`.
///
/// Exports: `log`, `default`.
/// Delegates to the global `console` object.
pub(crate) struct ConsoleModule;

impl ModuleDef for ConsoleModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("log")?;
        decl.declare("error")?;
        decl.declare("warn")?;
        decl.declare("info")?;
        decl.declare("debug")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &rquickjs::Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let console_global: Object<'js> = globals.get("console")?;

        let log_fn: Function<'js> = console_global.get("log")?;
        exports.export("log", log_fn.clone())?;

        let error_fn: Function<'js> = console_global.get("error")?;
        exports.export("error", error_fn.clone())?;

        let warn_fn: Function<'js> = console_global.get("warn")?;
        exports.export("warn", warn_fn.clone())?;

        let info_fn: Function<'js> = console_global.get("info")?;
        exports.export("info", info_fn.clone())?;

        let debug_fn: Function<'js> = console_global.get("debug")?;
        exports.export("debug", debug_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("log", log_fn)?;
        default_obj.set("error", error_fn)?;
        default_obj.set("warn", warn_fn)?;
        default_obj.set("info", info_fn)?;
        default_obj.set("debug", debug_fn)?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

/// Native module definition for `simulacra:process`.
///
/// Exports: `env`, `cwd`, `exit`, `default`.
/// Delegates to the global `process` object.
pub(crate) struct ProcessModule;

impl ModuleDef for ProcessModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("env")?;
        decl.declare("cwd")?;
        decl.declare("exit")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &rquickjs::Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let globals = ctx.globals();
        let process_global: Object<'js> = globals.get("process")?;

        let env_obj: Object<'js> = process_global.get("env")?;
        exports.export("env", env_obj.clone())?;

        let cwd_fn: Function<'js> = process_global.get("cwd")?;
        exports.export("cwd", cwd_fn.clone())?;

        let exit_fn: Function<'js> = process_global.get("exit")?;
        exports.export("exit", exit_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("env", env_obj)?;
        default_obj.set("cwd", cwd_fn)?;
        default_obj.set("exit", exit_fn)?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

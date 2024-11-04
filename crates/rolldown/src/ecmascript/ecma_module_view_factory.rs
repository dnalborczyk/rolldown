use arcstr::ArcStr;
use oxc::{
  index::IndexVec,
  semantic::{ScopeTree, SymbolTable},
};
use rolldown_common::{
  side_effects::{DeterminedSideEffects, HookSideEffects},
  AstScopes, EcmaView, EcmaViewMeta, ImportRecordIdx, ModuleDefFormat, ModuleId, ModuleIdx,
  ModuleType, RawImportRecord, SymbolRef, SymbolRefDbForModule, TreeshakeOptions,
};
use rolldown_ecmascript::EcmaAst;
use rolldown_error::BuildResult;
use rolldown_utils::{ecma_script::legitimize_identifier_name, path_ext::PathExt};
use sugar_path::SugarPath;

use crate::{
  ast_scanner::{AstScanner, ScanResult},
  types::module_factory::{CreateModuleContext, CreateModuleViewArgs},
  utils::{
    make_ast_symbol_and_scope::make_ast_scopes_and_symbols,
    parse_to_ecma_ast::{parse_to_ecma_ast, ParseToEcmaAstResult},
  },
};

fn scan_ast(
  module_idx: ModuleIdx,
  id: &ArcStr,
  ast: &mut EcmaAst,
  symbols: SymbolTable,
  scopes: ScopeTree,
  module_def_format: ModuleDefFormat,
) -> BuildResult<(AstScopes, ScanResult, SymbolRef)> {
  let (symbol_table, ast_scopes) = make_ast_scopes_and_symbols(symbols, scopes);
  let module_id = ModuleId::new(ArcStr::clone(id));
  let repr_name = module_id.as_path().representative_file_name();
  let repr_name = legitimize_identifier_name(&repr_name);

  let scanner = AstScanner::new(
    module_idx,
    &ast_scopes,
    symbol_table,
    &repr_name,
    module_def_format,
    ast.source(),
    &module_id,
    ast.comments(),
  );
  let namespace_object_ref = scanner.namespace_object_ref;
  let scan_result = scanner.scan(ast.program())?;

  Ok((ast_scopes, scan_result, namespace_object_ref))
}
pub struct CreateEcmaViewReturn {
  pub view: EcmaView,
  pub raw_import_records: IndexVec<ImportRecordIdx, RawImportRecord>,
  pub ast: EcmaAst,
  pub symbols: SymbolRefDbForModule,
}

#[allow(clippy::too_many_lines)]
pub async fn create_ecma_view<'any>(
  ctx: &mut CreateModuleContext<'any>,
  args: CreateModuleViewArgs,
) -> BuildResult<CreateEcmaViewReturn> {
  let id = ModuleId::new(ArcStr::clone(&ctx.resolved_id.id));
  let stable_id = id.stabilize(&ctx.options.cwd);

  let parse_result = parse_to_ecma_ast(
    ctx.plugin_driver,
    ctx.resolved_id.id.as_path(),
    &stable_id,
    ctx.options,
    &ctx.module_type,
    args.source.clone(),
    ctx.replace_global_define_config.as_ref(),
  )?;

  let ParseToEcmaAstResult { mut ast, symbol_table, scope_tree, has_lazy_export, warning } =
    parse_result;

  ctx.warnings.extend(warning);

  let (scope, scan_result, namespace_object_ref) = scan_ast(
    ctx.module_index,
    &ctx.resolved_id.id,
    &mut ast,
    symbol_table,
    scope_tree,
    ctx.resolved_id.module_def_format,
  )?;

  let ScanResult {
    named_imports,
    named_exports,
    stmt_infos,
    import_records,
    default_export_ref,
    imports,
    exports_kind,
    warnings: scan_warnings,
    has_eval,
    errors,
    ast_usage,
    symbol_ref_db,
    self_referenced_class_decl_symbol_ids,
    has_star_exports,
  } = scan_result;
  if !errors.is_empty() {
    return Err(errors.into());
  }
  ctx.warnings.extend(scan_warnings);

  let imported_ids = vec![];
  let dynamically_imported_ids = vec![];

  // The side effects priority is:
  // 1. Hook side effects
  // 2. Package.json side effects
  // 3. Analyzed side effects
  // We should skip the `check_side_effects_for` if the hook side effects is not `None`.
  let lazy_check_side_effects = || {
    if matches!(ctx.module_type, ModuleType::Css) {
      // CSS modules are considered to have side effects by default
      return DeterminedSideEffects::Analyzed(true);
    }
    ctx
      .resolved_id
      .package_json
      .as_ref()
      .and_then(|p| {
        // the glob expr is based on parent path of package.json, which is package path
        // so we should use the relative path of the module to package path
        let module_path_relative_to_package = id.as_path().relative(p.path.parent()?);
        p.check_side_effects_for(&module_path_relative_to_package.to_string_lossy())
          .map(DeterminedSideEffects::UserDefined)
      })
      .unwrap_or_else(|| {
        let analyzed_side_effects = stmt_infos.iter().any(|stmt_info| stmt_info.side_effect);
        DeterminedSideEffects::Analyzed(analyzed_side_effects)
      })
  };
  let side_effects = match args.hook_side_effects {
    Some(side_effects) => match side_effects {
      HookSideEffects::True => lazy_check_side_effects(),
      HookSideEffects::False => DeterminedSideEffects::UserDefined(false),
      HookSideEffects::NoTreeshake => DeterminedSideEffects::NoTreeshake,
    },
    // If user don't specify the side effects, we use fallback value from `option.treeshake.moduleSideEffects`;
    None => match ctx.options.treeshake {
      // Actually this convert is not necessary, just for passing type checking
      TreeshakeOptions::Boolean(false) => DeterminedSideEffects::NoTreeshake,
      TreeshakeOptions::Boolean(true) => unreachable!(),
      TreeshakeOptions::Option(ref opt) => {
        if opt.module_side_effects.resolve(&stable_id) {
          lazy_check_side_effects()
        } else {
          DeterminedSideEffects::UserDefined(false)
        }
      }
    },
  };

  // TODO: Should we check if there are `check_side_effects_for` returns false but there are side effects in the module?
  let view = EcmaView {
    source: ast.source().clone(),
    ecma_ast_idx: None,
    named_imports,
    named_exports,
    stmt_infos,
    imports,
    default_export_ref,
    scope,
    exports_kind,
    namespace_object_ref,
    def_format: ctx.resolved_id.module_def_format,
    sourcemap_chain: args.sourcemap_chain,
    import_records: IndexVec::default(),
    importers: vec![],
    dynamic_importers: vec![],
    imported_ids,
    dynamically_imported_ids,
    side_effects,
    ast_usage,
    self_referenced_class_decl_symbol_ids,
    meta: {
      let mut meta = EcmaViewMeta::default();
      meta.set_included(false);
      meta.set_eval(has_eval);
      meta.set_has_lazy_export(has_lazy_export);
      meta.set_has_star_exports(has_star_exports);
      meta
    },
    mutations: vec![],
  };

  Ok(CreateEcmaViewReturn { view, raw_import_records: import_records, ast, symbols: symbol_ref_db })
}

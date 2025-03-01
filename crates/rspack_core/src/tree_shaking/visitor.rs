use std::{
  collections::hash_map::Entry, collections::VecDeque, hash::Hash, path::PathBuf, sync::Arc,
};

use bitflags::bitflags;
use hashlink::LinkedHashMap;
use rspack_identifier::{IdentifierLinkedMap, IdentifierMap};
use rspack_symbol::{
  BetterId, IdOrMemExpr, IndirectTopLevelSymbol, IndirectType, StarSymbol, StarSymbolKind, Symbol,
  SymbolExt, SymbolFlag, SymbolType,
};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use sugar_path::SugarPath;
use swc_core::common::SyntaxContext;
use swc_core::common::{util::take::Take, Mark, GLOBALS};
use swc_core::ecma::ast::*;
use swc_core::ecma::atoms::{js_word, JsWord};
use swc_core::ecma::utils::{ExprCtx, ExprExt};
use swc_core::ecma::visit::{noop_visit_type, Visit, VisitWith};

use super::SideEffect;
// use swc_atoms::JsWord;
// use swc_common::{util::take::Take, Mark, GLOBALS};
// use swc_ecma_ast::*;
// use swc_ecma_visit::{noop_visit_type, Visit, VisitWith};
use super::{
  utils::{get_dynamic_import_string_literal, get_require_literal},
  BailoutFlag,
};
use crate::{
  CompilerOptions, Dependency, DependencyType, ModuleGraph, ModuleIdentifier, ModuleSyntax,
  ResolverFactory,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SymbolRef {
  Direct(Symbol),
  Indirect(IndirectTopLevelSymbol),
  /// uri
  Star(StarSymbol),
}

impl SymbolRef {
  pub fn module_identifier(&self) -> ModuleIdentifier {
    match self {
      SymbolRef::Direct(d) => d.uri(),
      SymbolRef::Indirect(i) => i.src(),
      SymbolRef::Star(s) => s.src(),
    }
  }

  pub fn importer(&self) -> ModuleIdentifier {
    match self {
      SymbolRef::Direct(d) => d.uri(),
      SymbolRef::Indirect(i) => i.importer,
      SymbolRef::Star(s) => s.module_ident(),
    }
  }
  /// Returns `true` if the symbol ref is [`Direct`].
  ///
  /// [`Direct`]: SymbolRef::Direct
  #[must_use]
  pub fn is_direct(&self) -> bool {
    matches!(self, Self::Direct(..))
  }

  /// Returns `true` if the symbol ref is [`Indirect`].
  ///
  /// [`Indirect`]: SymbolRef::Indirect
  #[must_use]
  pub fn is_indirect(&self) -> bool {
    matches!(self, Self::Indirect(..))
  }

  /// Returns `true` if the symbol ref is [`Star`].
  ///
  /// [`Star`]: SymbolRef::Star
  #[must_use]
  pub fn is_star(&self) -> bool {
    matches!(self, Self::Star(..))
  }

  pub fn is_skipable_symbol(&self) -> bool {
    match self {
      SymbolRef::Indirect(indirect) => !indirect.is_import(),
      SymbolRef::Star(StarSymbol {
        ty: StarSymbolKind::ReExportAll,
        ..
      }) => true,
      _ => false,
    }
  }
}

bitflags! {
  #[derive(Default)]
  struct AnalyzeState: u8 {
    const EXPORT_DECL = 1 << 0;
    const EXPORT_DEFAULT = 1 << 1;
    const ASSIGNMENT_LHS = 1 << 2;
    const ASSIGNMENT_RHS = 1 << 3;
    const STATIC_VAR_DECL = 1 << 4;
  }
}
#[derive(Debug)]
pub(crate) struct ModuleRefAnalyze<'a> {
  top_level_mark: Mark,
  unresolved_mark: Mark,
  helper_mark: Mark,
  module_identifier: ModuleIdentifier,
  module_graph: &'a ModuleGraph,
  /// Value of `export_map` must have type [SymbolRef::Direct]
  pub(crate) export_map: HashMap<JsWord, SymbolRef>,
  pub(crate) import_map: HashMap<BetterId, SymbolRef>,
  /// key is the module identifier, value is the corresponding export map
  /// This data structure is used for collecting reexport * from some module. e.g.
  /// ```js
  /// export * from './test.js'
  /// ```
  /// then inherit_exports_maps become, `{"test.js": {...test_js_export_map} }`
  // Use `IndexMap` to keep the insertion order
  pub inherit_export_maps: IdentifierLinkedMap<HashMap<JsWord, SymbolRef>>,
  current_body_owner_symbol_ext: Option<SymbolExt>,
  pub(crate) maybe_lazy_reference_map: HashMap<SymbolExt, HashSet<IdOrMemExpr>>,
  /// ```js
  /// The method
  /// ```
  pub(crate) immediate_evaluate_reference_map: HashMap<SymbolExt, HashSet<IdOrMemExpr>>,
  pub(crate) reachable_import_and_export: HashMap<JsWord, HashSet<SymbolRef>>,
  state: AnalyzeState,
  pub(crate) used_id_set: HashSet<IdOrMemExpr>,
  pub(crate) used_symbol_ref: HashSet<SymbolRef>,
  // This field is used for duplicated export default checking
  pub(crate) export_default_name: Option<JsWord>,
  module_syntax: ModuleSyntax,
  pub(crate) bail_out_module_identifiers: IdentifierMap<BailoutFlag>,
  pub(crate) resolver_factory: &'a Arc<ResolverFactory>,
  pub(crate) side_effects: SideEffect,
  pub(crate) options: &'a Arc<CompilerOptions>,
  pub(crate) has_side_effects_stmt: bool,
  unresolved_ctxt: SyntaxContext,
  pub(crate) potential_top_mark: HashSet<Mark>,
}

impl<'a> ModuleRefAnalyze<'a> {
  pub fn new(
    top_level_mark: Mark,
    unresolved_mark: Mark,
    helper_mark: Mark,
    uri: ModuleIdentifier,
    dep_to_module_identifier: &'a ModuleGraph,
    resolver_factory: &'a Arc<ResolverFactory>,
    options: &'a Arc<CompilerOptions>,
  ) -> Self {
    Self {
      top_level_mark,
      unresolved_mark,
      helper_mark,
      module_identifier: uri,
      module_graph: dep_to_module_identifier,
      export_map: HashMap::default(),
      import_map: HashMap::default(),
      inherit_export_maps: LinkedHashMap::default(),
      current_body_owner_symbol_ext: None,
      maybe_lazy_reference_map: HashMap::default(),
      reachable_import_and_export: HashMap::default(),
      state: AnalyzeState::empty(),
      used_id_set: HashSet::default(),
      used_symbol_ref: HashSet::default(),
      export_default_name: None,
      module_syntax: ModuleSyntax::empty(),
      bail_out_module_identifiers: IdentifierMap::default(),
      resolver_factory,
      side_effects: SideEffect::Analyze(true),
      immediate_evaluate_reference_map: HashMap::default(),
      options,
      has_side_effects_stmt: false,
      unresolved_ctxt: SyntaxContext::empty(),
      potential_top_mark: HashSet::from_iter([top_level_mark]),
    }
  }

  /// Some times we want to insert the reference anyway, even if `from == to`
  /// e.g.
  /// ```js
  /// import xxx from 'xxx'
  /// xxx.test = aaa;
  /// ```
  pub fn add_reference(&mut self, from: SymbolExt, to: IdOrMemExpr, force_insert: bool) {
    if matches!(&to, IdOrMemExpr::Id(to_id) if to_id == from.id()) && !force_insert {
      return;
    }
    if self
      .state
      .intersection(
        AnalyzeState::ASSIGNMENT_RHS | AnalyzeState::ASSIGNMENT_LHS | AnalyzeState::STATIC_VAR_DECL,
      )
      .bits()
      .count_ones()
      >= 1
    {
      match self.immediate_evaluate_reference_map.entry(from) {
        Entry::Occupied(mut occ) => {
          occ.get_mut().insert(to);
        }
        Entry::Vacant(vac) => {
          vac.insert(HashSet::from_iter([to]));
        }
      }
    } else {
      match self.maybe_lazy_reference_map.entry(from) {
        Entry::Occupied(mut occ) => {
          occ.get_mut().insert(to);
        }
        Entry::Vacant(vac) => {
          vac.insert(HashSet::from_iter([to]));
        }
      }
    }
  }

  /// Collecting all reachable import binding from given start binding
  /// when a export has been used from other module, we need to get all
  /// reachable import and export(defined in the same module)
  /// in rest of scenario we only count binding imported from other module.
  pub fn get_all_import_or_export(&self, start: BetterId, only_import: bool) -> HashSet<SymbolRef> {
    let mut seen: HashSet<IdOrMemExpr> = HashSet::default();
    let mut q: VecDeque<IdOrMemExpr> = VecDeque::from_iter([IdOrMemExpr::Id(start)]);
    while let Some(cur) = q.pop_front() {
      if seen.contains(&cur) {
        continue;
      }
      let id = cur.get_id();
      if let Some(ref_list) = self.maybe_lazy_reference_map.get(&id.clone().into()) {
        q.extend(ref_list.clone());
      }
      seen.insert(cur);
    }
    // dbg!(&start, &seen, &self.reference_map);
    return seen
      .iter()
      .filter_map(|id_or_mem_expr| match id_or_mem_expr {
        IdOrMemExpr::Id(id) => {
          let ret = self.import_map.get(id).cloned().or_else(|| {
            if only_import {
              None
            } else {
              match self.export_map.get(&id.atom) {
                Some(sym_ref @ SymbolRef::Direct(sym)) => {
                  if sym.id() == id {
                    Some(sym_ref.clone())
                  } else {
                    None
                  }
                }
                _ => None,
              }
            }
          });
          ret
        }
        IdOrMemExpr::MemberExpr { object, property } => {
          self.import_map.get(object).map(|sym_ref| match sym_ref {
            SymbolRef::Direct(_) | SymbolRef::Indirect(_) => sym_ref.clone(),
            SymbolRef::Star(uri) => SymbolRef::Indirect(IndirectTopLevelSymbol::new(
              uri.src(),
              self.module_identifier,
              IndirectType::Import(property.clone(), None),
            )),
          })
        }
      })
      .collect();
  }

  fn generate_default_ident(&self) -> Ident {
    let mut default_ident = Ident::dummy();
    default_ident.sym = "default".into();
    default_ident.span = default_ident.span.apply_mark(self.top_level_mark);
    default_ident
  }

  fn check_commonjs_feature(&mut self, obj: &Ident, prop: &str) {
    if self.state.contains(AnalyzeState::ASSIGNMENT_LHS)
      && ((&obj.sym == "module" && prop == "exports") || &obj.sym == "exports")
    {
      match self
        .bail_out_module_identifiers
        .entry(self.module_identifier)
      {
        Entry::Occupied(mut occ) => {
          *occ.get_mut() |= BailoutFlag::COMMONJS_EXPORTS;
        }
        Entry::Vacant(vac) => {
          vac.insert(BailoutFlag::COMMONJS_EXPORTS);
        }
      }
    }
  }
}

impl<'a> Visit for ModuleRefAnalyze<'a> {
  noop_visit_type!();
  fn visit_program(&mut self, node: &Program) {
    assert!(GLOBALS.is_set());
    self.unresolved_ctxt = self.unresolved_ctxt.apply_mark(self.unresolved_mark);
    node.visit_children_with(self);
    // calc reachable imports for each export symbol defined in current module
    for (key, symbol) in self.export_map.iter() {
      match symbol {
        // At this time uri of symbol will always equal to `self.module_identifier`
        SymbolRef::Direct(symbol) => {
          let reachable_import_and_export =
            self.get_all_import_or_export(symbol.id().clone(), false);
          if key != &symbol.id().atom {
            // export {xxx as xxx}
            self.reachable_import_and_export.insert(
              symbol.id().atom.clone(),
              reachable_import_and_export.clone(),
            );
          }
          self
            .reachable_import_and_export
            .insert(key.clone(), reachable_import_and_export);
        }
        // ignore any indirect symbol, because it will not generate binding, the reachable exports will
        // be calculated in the module where it is defined
        SymbolRef::Indirect(_) | SymbolRef::Star(_) => {}
      }
    }
    // Any var declaration has reference a symbol from other module, it is marked as used
    // Because the symbol import from other module possibly has side effect
    let side_effect_symbol_list = self
      .maybe_lazy_reference_map
      .iter()
      .flat_map(|(symbol, ref_list)| {
        // it class decl, fn decl is lazy they don't immediately generate side effects unless they are called,
        // Or constructed. The init of var decl will evaluate except rhs is function expr or arrow expr.
        if !symbol.flag.contains(SymbolFlag::VAR_DECL)
          && !symbol.flag.contains(SymbolFlag::EXPORT_DEFAULT)
        {
          vec![]
        } else {
          // like
          // ```js
          // export var a = function () {}
          // ```
          // a is still lazy
          if symbol
            .flag
            .intersection(
              SymbolFlag::FUNCTION_EXPR
                | SymbolFlag::ARROW_EXPR
                | SymbolFlag::CLASS_EXPR
                | SymbolFlag::ALIAS,
            )
            .bits()
            .count_ones()
            >= 1
          {
            return vec![];
          }
          ref_list
            .iter()
            .flat_map(|ref_id| {
              // Only used id imported from other module would generate a side effects.
              let id = match ref_id {
                IdOrMemExpr::Id(ref id) => id,
                // TODO: inspect namespace access
                IdOrMemExpr::MemberExpr { object, .. } => object,
              };
              // dbg!(&id);
              let ret = self.import_map.get(id);
              match ret {
                Some(ret) => HashSet::from_iter([ret.clone()]),
                None => self.get_all_import_or_export(id.clone(), true),
              }
            })
            .collect::<Vec<_>>()
        }
      })
      .collect::<Vec<_>>();
    self.used_symbol_ref.extend(side_effect_symbol_list);
    let side_effect_symbol_list = self
      .immediate_evaluate_reference_map
      .iter()
      .flat_map(|(_, ref_list)| {
        // it class decl, fn decl is lazy they don't immediately generate side effects unless they are called,
        // Or constructed. The init of var decl will evaluate except rhs is function expr or arrow expr.
        ref_list
          .iter()
          .flat_map(|ref_id| {
            // Only used id imported from other module would generate a side effects.
            let id = match ref_id {
              IdOrMemExpr::Id(ref id) => id,
              // TODO: inspect namespace access
              IdOrMemExpr::MemberExpr { object, .. } => object,
            };
            let ret = self.import_map.get(id);
            match ret {
              Some(ret) => HashSet::from_iter([ret.clone()]),
              None => self.get_all_import_or_export(id.clone(), true),
            }
          })
          .collect::<Vec<_>>()
      })
      .collect::<Vec<_>>();
    self.used_symbol_ref.extend(side_effect_symbol_list);
    // dbg!(&self.used_id_set);
    // all reachable export from used symbol in current module
    for used_id in &self.used_id_set {
      match used_id {
        IdOrMemExpr::Id(id) => {
          let reachable_import = self.get_all_import_or_export(id.clone(), true);
          self.used_symbol_ref.extend(reachable_import);
        }
        IdOrMemExpr::MemberExpr { object, property } => match self.import_map.get(object) {
          Some(SymbolRef::Star(uri)) => {
            self
              .used_symbol_ref
              .insert(SymbolRef::Indirect(IndirectTopLevelSymbol::new(
                uri.src(),
                self.module_identifier,
                IndirectType::Import(property.clone(), None),
              )));
          }
          _ => {
            let reachable_import = self.get_all_import_or_export(object.clone(), true);
            self.used_symbol_ref.extend(reachable_import);
          }
        },
      }
    }

    let side_effects_option = self.options.optimization.side_effects;
    if side_effects_option.is_enable() {
      self.side_effects = self.get_side_effects_from_config().unwrap_or_else(|| {
        if side_effects_option.is_true() {
          SideEffect::Analyze(self.has_side_effects_stmt)
        } else {
          // side_effects_option must be `flag` here
          SideEffect::Configuration(true)
        }
      });
    }
  }

  fn visit_module(&mut self, node: &Module) {
    // prescan import decl
    for module_item in &node.body {
      if is_import_decl(module_item) {
        module_item.visit_with(self);
      }
    }
    for module_item in &node.body {
      if !is_import_decl(module_item) {
        self.analyze_stmt_side_effects(module_item);
        module_item.visit_with(self);
      }
    }
  }

  fn visit_ident(&mut self, node: &Ident) {
    let id: BetterId = node.to_id().into();
    let mark = id.ctxt.outer();

    if self.potential_top_mark.contains(&mark) {
      match self.current_body_owner_symbol_ext {
        Some(ref body_owner_symbol_ext) if body_owner_symbol_ext.id() != &id => {
          self.add_reference(body_owner_symbol_ext.clone(), IdOrMemExpr::Id(id), false);
        }
        None => {
          self.used_id_set.insert(IdOrMemExpr::Id(id));
        }
        _ => {}
      }
    }
  }

  fn visit_module_item(&mut self, node: &ModuleItem) {
    match node {
      ModuleItem::ModuleDecl(decl) => match decl {
        ModuleDecl::Import(import) => {
          let src = &import.src.value;
          let resolved_uri = match self.resolve_module_identifier(src, &DependencyType::EsmImport) {
            Some(module_identifier) => module_identifier,
            None => {
              // TODO: Ignore for now because swc helper interference.
              return;
            }
          };
          let resolved_uri_ukey = *resolved_uri;
          import
            .specifiers
            .iter()
            .for_each(|specifier| match specifier {
              ImportSpecifier::Named(named) => {
                let imported = named.imported.as_ref().map(|imported| match imported {
                  ModuleExportName::Ident(ident) => ident.sym.clone(),
                  ModuleExportName::Str(str) => str.value.clone(),
                });

                let local = named.local.sym.clone();

                let symbol_ref = SymbolRef::Indirect(IndirectTopLevelSymbol::new(
                  resolved_uri_ukey,
                  self.module_identifier,
                  IndirectType::Import(local, imported),
                ));

                self.add_import(named.local.to_id().into(), symbol_ref);
              }
              ImportSpecifier::Default(default) => {
                self.add_import(
                  default.local.to_id().into(),
                  SymbolRef::Indirect(IndirectTopLevelSymbol::new(
                    resolved_uri_ukey,
                    self.module_identifier,
                    IndirectType::ImportDefault(default.local.sym.clone()),
                  )),
                );
              }
              ImportSpecifier::Namespace(namespace) => {
                self.add_import(
                  namespace.local.to_id().into(),
                  SymbolRef::Star(StarSymbol::new(
                    resolved_uri_ukey,
                    namespace.local.sym.clone(),
                    self.module_identifier,
                    StarSymbolKind::ImportAllAs,
                  )),
                );
              }
            });
        }
        ModuleDecl::ExportDecl(decl) => match &decl.decl {
          Decl::Class(class) => {
            class.visit_with(self);
            self.add_export(
              class.ident.sym.clone(),
              SymbolRef::Direct(Symbol::new(
                self.module_identifier,
                class.ident.to_id().into(),
                SymbolType::Define,
              )),
            );
          }
          Decl::Fn(function) => {
            function.visit_with(self);
            self.add_export(
              function.ident.sym.clone(),
              SymbolRef::Direct(Symbol::new(
                self.module_identifier,
                function.ident.to_id().into(),
                SymbolType::Define,
              )),
            );
          }
          Decl::Var(var) => {
            self.state |= AnalyzeState::EXPORT_DECL;
            var.visit_with(self);
            self.state.remove(AnalyzeState::EXPORT_DECL);
          }
          Decl::TsInterface(_) | Decl::TsTypeAlias(_) | Decl::TsEnum(_) | Decl::TsModule(_) => {
            todo!()
          }
        },
        ModuleDecl::ExportNamed(named_export) => {
          self.analyze_named_export(named_export);
        }
        ModuleDecl::ExportDefaultDecl(decl) => {
          decl.visit_with(self);
        }
        ModuleDecl::ExportDefaultExpr(expr) => {
          expr.visit_with(self);
        }

        ModuleDecl::ExportAll(export_all) => {
          let resolved_uri = match self
            .resolve_module_identifier(&export_all.src.value, &DependencyType::EsmExport)
          {
            Some(module_identifier) => module_identifier,
            None => {
              // TODO: ignore for now, or three copy js will failed
              return;
            }
          };
          let resolved_uri_key = *resolved_uri;
          self
            .inherit_export_maps
            .insert(resolved_uri_key, HashMap::default());
        }
        ModuleDecl::TsImportEquals(_)
        | ModuleDecl::TsExportAssignment(_)
        | ModuleDecl::TsNamespaceExport(_) => {
          // TODO: ignore ts related syntax visit for now
        }
      },
      ModuleItem::Stmt(stmt) => {
        stmt.visit_children_with(self);
      }
    }
  }

  fn visit_export_default_expr(&mut self, node: &ExportDefaultExpr) {
    let before_owner_extend_symbol = self.current_body_owner_symbol_ext.clone();
    let default_ident: BetterId = self.generate_default_ident().to_id().into();

    self.add_export(
      default_ident.atom.clone(),
      SymbolRef::Direct(Symbol::new(
        self.module_identifier,
        default_ident.clone(),
        SymbolType::Define,
      )),
    );
    match self.export_default_name {
      Some(_) => {
        // TODO: Better diagnostic
        panic!(
          "Duplicate export default (export_default_expr) in {}",
          self.module_identifier
        )
      }
      None => {
        self.export_default_name = Some("default".into());
      }
    }
    let mut symbol_ext: SymbolExt = default_ident.into();
    symbol_ext.flag.insert(SymbolFlag::EXPORT_DEFAULT);
    match node.expr {
      box Expr::Fn(_) => symbol_ext.flag.insert(SymbolFlag::FUNCTION_EXPR),
      box Expr::Arrow(_) => symbol_ext.flag.insert(SymbolFlag::ARROW_EXPR),
      box Expr::Ident(_) => symbol_ext.flag.insert(SymbolFlag::ALIAS),
      _ => {}
    };
    self.current_body_owner_symbol_ext = Some(symbol_ext);
    node.visit_children_with(self);
    self.current_body_owner_symbol_ext = before_owner_extend_symbol;
  }

  fn visit_assign_expr(&mut self, node: &AssignExpr) {
    // TODO: assign should have body ext too
    let before_owner_extend_symbol = self.current_body_owner_symbol_ext.clone();
    let target = if before_owner_extend_symbol.is_none() {
      let target = first_ident_of_assign_lhs(node);
      target.and_then(|target| {
        if target.1.outer() == self.top_level_mark {
          Some(target)
        } else {
          None
        }
      })
    } else {
      None
    };
    let valid_assign_target = target.is_some();
    if let Some(target) = target {
      self.current_body_owner_symbol_ext = Some(SymbolExt {
        id: target.into(),
        flag: SymbolFlag::empty(),
      });
    }

    self.state.insert(AnalyzeState::ASSIGNMENT_LHS);
    node.left.visit_with(self);
    self.state.remove(AnalyzeState::ASSIGNMENT_LHS);
    if valid_assign_target {
      self.state.insert(AnalyzeState::ASSIGNMENT_RHS);
    }
    node.right.visit_with(self);
    self.state.remove(AnalyzeState::ASSIGNMENT_RHS);
    self.current_body_owner_symbol_ext = before_owner_extend_symbol;
  }

  fn visit_class_prop(&mut self, node: &ClassProp) {
    node.key.visit_with(self);
    if let Some(ref expr) = node.value {
      match expr {
        box Expr::Fn(_) | box Expr::Arrow(_) => {
          expr.visit_with(self);
        }
        _ => {
          self.state.insert(AnalyzeState::STATIC_VAR_DECL);
          expr.visit_with(self);
          self.state.remove(AnalyzeState::STATIC_VAR_DECL);
        }
      }
    }
  }

  fn visit_member_expr(&mut self, node: &MemberExpr) {
    match (&*node.obj, &node.prop) {
      // a.b
      (Expr::Ident(obj), MemberProp::Ident(prop)) => {
        self.check_commonjs_feature(obj, &prop.sym);
        let id: BetterId = obj.to_id().into();
        let mark = id.ctxt.outer();

        if mark == self.top_level_mark {
          let member_expr = IdOrMemExpr::MemberExpr {
            object: id.clone(),
            property: prop.sym.clone(),
          };
          match self.current_body_owner_symbol_ext {
            Some(ref body_owner_symbol_ext) => {
              if body_owner_symbol_ext.id() != &id {
                self.add_reference(body_owner_symbol_ext.clone(), member_expr, false);
              } else if self.state.contains(AnalyzeState::ASSIGNMENT_LHS) {
                self.add_reference(body_owner_symbol_ext.clone(), member_expr, true);
              }
            }
            None => {
              self.used_id_set.insert(member_expr);
            }
          }
        }
      }
      // TODO: Do we need to consider such scenario ?
      // obj[`test`] use a template literal but actually is a pure string lit
      // obj['prop']
      (
        Expr::Ident(obj),
        MemberProp::Computed(ComputedPropName {
          expr: box Expr::Lit(Lit::Str(Str { value, .. })),
          ..
        }),
      ) => {
        self.check_commonjs_feature(obj, value);

        let id: BetterId = obj.to_id().into();
        let mark = id.ctxt.outer();
        if mark == self.top_level_mark {
          let member_expr = IdOrMemExpr::MemberExpr {
            object: id.clone(),
            property: value.clone(),
          };
          match self.current_body_owner_symbol_ext {
            Some(ref body_owner_symbol_ext) if body_owner_symbol_ext.id() != &id => {
              self.add_reference(body_owner_symbol_ext.clone(), member_expr, false);
            }
            None => {
              self.used_id_set.insert(member_expr);
            }
            _ => {}
          }
        }
      }
      _ => {
        node.visit_children_with(self);
      }
    }
  }

  fn visit_export_default_decl(&mut self, node: &ExportDefaultDecl) {
    self.state.insert(AnalyzeState::EXPORT_DEFAULT);
    match &node.decl {
      DefaultDecl::Class(_) | DefaultDecl::Fn(_) => {
        node.visit_children_with(self);
      }
      DefaultDecl::TsInterfaceDecl(_) => {
        unreachable!("We have been converted Typescript to javascript already")
      }
    }
    self.state.remove(AnalyzeState::EXPORT_DEFAULT);
  }

  fn visit_class_expr(&mut self, node: &ClassExpr) {
    if self.state.contains(AnalyzeState::EXPORT_DEFAULT) {
      self.state.remove(AnalyzeState::EXPORT_DEFAULT);
      let default_ident = self.generate_default_ident();
      self.add_export(
        default_ident.sym.clone(),
        SymbolRef::Direct(Symbol::new(
          self.module_identifier,
          default_ident.to_id().into(),
          SymbolType::Define,
        )),
      );
      let body_owner_extend_symbol: SymbolExt = match self.export_default_name {
        Some(_) => {
          // TODO: Better diagnostic
          panic!(
            "Duplicate export default(class_expr) in {}",
            self.module_identifier
          )
        }
        None => {
          let symbol_flag = SymbolFlag::EXPORT_DEFAULT | SymbolFlag::CLASS_EXPR;
          let symbol_ext: SymbolExt = if let Some(ident) = &node.ident {
            let renamed_symbol_ext = SymbolExt::new(ident.to_id().into(), symbol_flag);
            let default_ident_ext: SymbolExt = BetterId::from(default_ident.to_id()).into();
            self.add_reference(
              default_ident_ext.clone(),
              IdOrMemExpr::Id(renamed_symbol_ext.id.clone()),
              false,
            );
            self.add_reference(
              renamed_symbol_ext.clone(),
              IdOrMemExpr::Id(default_ident_ext.id),
              false,
            );
            renamed_symbol_ext
          } else {
            SymbolExt::new(default_ident.to_id().into(), symbol_flag)
          };
          self.export_default_name = Some(symbol_ext.id().atom.clone());
          symbol_ext
        }
      };
      let before_owner_extend_symbol = self.current_body_owner_symbol_ext.clone();
      self.current_body_owner_symbol_ext = Some(body_owner_extend_symbol);
      node.class.visit_with(self);
      self.current_body_owner_symbol_ext = before_owner_extend_symbol;
    } else {
      // if the class expr is not inside a default expr, it will not
      // generate a binding.
      node.class.visit_with(self);
    }
  }
  // fn visit_span(&mut self, span: &Span) {}
  fn visit_call_expr(&mut self, node: &CallExpr) {
    // TODO: module.exports, exports.xxxxx
    if let Some(require_lit) = get_require_literal(node, self.unresolved_mark) {
      match self
        .resolve_module_identifier(&require_lit, &DependencyType::CjsRequire)
        .copied()
      {
        Some(module_identifier) => {
          match self.bail_out_module_identifiers.entry(module_identifier) {
            Entry::Occupied(mut occ) => {
              *occ.get_mut() |= BailoutFlag::COMMONJS_REQUIRE;
            }
            Entry::Vacant(vac) => {
              vac.insert(BailoutFlag::COMMONJS_REQUIRE);
            }
          }
        }
        None => {
          eprintln!(
            "Can't resolve require {} in {}",
            require_lit, self.module_identifier
          );
        }
      };
      self.module_syntax.insert(ModuleSyntax::COMMONJS);
    } else if let Some(import_str) = get_dynamic_import_string_literal(node) {
      match self
        .resolve_module_identifier(&import_str, &DependencyType::DynamicImport)
        .copied()
      {
        Some(module_identifier) => {
          match self.bail_out_module_identifiers.entry(module_identifier) {
            Entry::Occupied(mut occ) => {
              *occ.get_mut() |= BailoutFlag::DYNAMIC_IMPORT;
            }
            Entry::Vacant(vac) => {
              vac.insert(BailoutFlag::DYNAMIC_IMPORT);
            }
          }
        }
        None => {
          eprintln!(
            "Can't resolve dynamic import {} in {}",
            import_str, self.module_identifier
          );
        }
      };
    } else {
      node.visit_children_with(self);
    }
  }

  fn visit_fn_expr(&mut self, node: &FnExpr) {
    if self.state.contains(AnalyzeState::EXPORT_DEFAULT) {
      self.state.remove(AnalyzeState::EXPORT_DEFAULT);
      let default_ident = self.generate_default_ident();
      self.add_export(
        default_ident.sym.clone(),
        SymbolRef::Direct(Symbol::new(
          self.module_identifier,
          default_ident.to_id().into(),
          SymbolType::Define,
        )),
      );
      let body_owner_extend_symbol: SymbolExt = match self.export_default_name {
        Some(_) => {
          // TODO: Better diagnostic
          panic!(
            "Duplicate export default(fn_expr) in {}",
            self.module_identifier
          )
        }
        None => {
          let symbol_flag = SymbolFlag::EXPORT_DEFAULT | SymbolFlag::FUNCTION_EXPR;
          let symbol_ext: SymbolExt = if let Some(ident) = &node.ident {
            let symbol_ext = SymbolExt::new(ident.to_id().into(), symbol_flag);
            // considering default export has bind to new symbol e.g.
            // ```js
            // export default function test() {
            // }
            // let result = test();
            // ``

            self.add_reference(
              symbol_ext.clone(),
              IdOrMemExpr::Id(default_ident.to_id().into()),
              false,
            );
            self.add_reference(
              BetterId::from(default_ident.to_id()).into(),
              IdOrMemExpr::Id(symbol_ext.id.clone()),
              false,
            );
            symbol_ext
          } else {
            SymbolExt::new(default_ident.to_id().into(), symbol_flag)
          };
          self.export_default_name = Some(symbol_ext.id().atom.clone());
          symbol_ext
        }
      };
      let before_owner_extend_symbol = self.current_body_owner_symbol_ext.clone();

      self.current_body_owner_symbol_ext = Some(body_owner_extend_symbol);
      node.function.visit_with(self);
      self.current_body_owner_symbol_ext = before_owner_extend_symbol;
    } else {
      // if the function expr is not inside a default expr, it will not
      // generate a binding.
      node.function.visit_with(self);
    }
  }

  fn visit_class_decl(&mut self, node: &ClassDecl) {
    let id: BetterId = node.ident.to_id().into();
    let mark = id.ctxt.outer();
    let old_region = self.current_body_owner_symbol_ext.clone();
    if mark == self.top_level_mark {
      self.current_body_owner_symbol_ext = Some(id.into());
    }
    node.visit_children_with(self);
    self.current_body_owner_symbol_ext = old_region;
  }

  fn visit_fn_decl(&mut self, node: &FnDecl) {
    let id: BetterId = node.ident.to_id().into();
    let mark = id.ctxt.outer();
    let before_symbol_ext = self.current_body_owner_symbol_ext.clone();
    if mark == self.top_level_mark {
      self.current_body_owner_symbol_ext = Some(id.into());
    }
    node.function.visit_with(self);
    self.current_body_owner_symbol_ext = before_symbol_ext;
  }

  fn visit_var_decl(&mut self, node: &VarDecl) {
    for ele in node.decls.iter() {
      let lhs: BetterId = match &ele.name {
        Pat::Ident(ident) => ident.to_id().into(),
        Pat::Array(_)
        | Pat::Rest(_)
        | Pat::Object(_)
        | Pat::Assign(_)
        | Pat::Invalid(_)
        | Pat::Expr(_) => {
          // TODO:
          ele.visit_with(self);
          continue;
        }
      };
      let is_export = self.state.contains(AnalyzeState::EXPORT_DECL);
      if is_export && lhs.ctxt.outer() == self.top_level_mark {
        self.add_export(
          lhs.atom.clone(),
          SymbolRef::Direct(Symbol::new(
            self.module_identifier,
            lhs.clone(),
            SymbolType::Define,
          )),
        );
      }
      if let Some(ref init) = ele.init && lhs.ctxt.outer() == self.top_level_mark {
        let mut symbol_ext = SymbolExt::new(lhs, SymbolFlag::VAR_DECL);
        match init {
            box Expr::Fn(_) => symbol_ext.flag.insert(SymbolFlag::FUNCTION_EXPR),
            box Expr::Arrow(_) => symbol_ext.flag.insert(SymbolFlag::ARROW_EXPR),
            _ => {}
        };
        if is_export {
          symbol_ext.flag.insert(SymbolFlag::EXPORT);
        }
        let before_symbol_ext = self.current_body_owner_symbol_ext.clone();
        self.current_body_owner_symbol_ext = Some(symbol_ext);
        init.visit_with(self);
        self.current_body_owner_symbol_ext = before_symbol_ext;
      } else {
        ele.init.visit_with(self);
      }
    }
  }
  fn visit_decl(&mut self, node: &Decl) {
    match node {
      Decl::TsInterface(_) | Decl::TsTypeAlias(_) | Decl::TsEnum(_) | Decl::TsModule(_) => {
        // TODO: Ignore ts related tree-shaking for now.
      }
      Decl::Class(_) | Decl::Fn(_) | Decl::Var(_) => {
        node.visit_children_with(self);
      }
    }
  }
}

impl<'a> ModuleRefAnalyze<'a> {
  fn get_side_effects_from_config(&mut self) -> Option<SideEffect> {
    let resource_path = self
      .module_graph
      .module_by_identifier(&self.module_identifier)
      .and_then(|module| module.as_normal_module())
      .map(|normal_module| &normal_module.resource_resolved_data().resource_path)?;

    // sideEffects in module.rule has higher priority,
    // we could early return if we match a rule.
    for rule in self.options.module.rules.iter() {
      let module_side_effects = match rule.side_effects {
        Some(s) => s,
        None => continue,
      };
      match rule.test {
        Some(ref test_rule) => {
          if !test_rule.is_match(&resource_path.to_string_lossy()) {
            continue;
          }
        }
        None => {
          continue;
        }
      }
      return Some(SideEffect::Analyze(module_side_effects));
    }
    let module_path = PathBuf::from(resource_path);
    let (mut package_json_path, side_effects) = self
      .resolver_factory
      .resolver
      .0
      .load_side_effects(module_path.as_path())
      .ok()??;
    let side_effects = side_effects?;

    package_json_path.pop();
    let package_path = package_json_path;

    let relative_path = module_path.relative(package_path);
    let side_effects = Some(get_side_effects_from_package_json(
      side_effects,
      relative_path,
    ));

    side_effects.map(SideEffect::Configuration)
  }
}

pub fn get_side_effects_from_package_json(
  side_effects: nodejs_resolver::SideEffects,
  relative_path: PathBuf,
) -> bool {
  match side_effects {
    nodejs_resolver::SideEffects::Bool(s) => s,
    nodejs_resolver::SideEffects::String(s) => {
      let trim_start = s.trim_start_matches("./");
      let normalized_glob = if trim_start.contains('/') {
        trim_start.to_string()
      } else {
        String::from("**/") + trim_start
      };
      glob_match::glob_match(
        &normalized_glob,
        relative_path.to_string_lossy().trim_start_matches("./"),
      )
    }
    nodejs_resolver::SideEffects::Array(patterns) => patterns
      .iter()
      .any(|pattern| glob_match::glob_match(pattern, &relative_path.to_string_lossy())),
  }
}

impl<'a> ModuleRefAnalyze<'a> {
  /// If we find a stmt that has side effects, we will skip the rest of the stmts.
  /// And mark the module as having side effects.
  fn analyze_stmt_side_effects(&mut self, ele: &ModuleItem) {
    if !self.has_side_effects_stmt {
      match ele {
        ModuleItem::Stmt(Stmt::If(stmt)) => {
          if !is_pure_expression(&stmt.test, self.unresolved_ctxt) {
            self.has_side_effects_stmt = true;
          }
        }
        ModuleItem::Stmt(Stmt::While(stmt)) => {
          if !is_pure_expression(&stmt.test, self.unresolved_ctxt) {
            self.has_side_effects_stmt = true;
          }
        }
        ModuleItem::Stmt(Stmt::DoWhile(stmt)) => {
          if !is_pure_expression(&stmt.test, self.unresolved_ctxt) {
            self.has_side_effects_stmt = true;
          }
        }
        ModuleItem::Stmt(Stmt::For(stmt)) => {
          let pure_init = match stmt.init {
            Some(ref init) => match init {
              VarDeclOrExpr::VarDecl(decl) => is_pure_var_decl(decl, self.unresolved_ctxt),
              VarDeclOrExpr::Expr(expr) => is_pure_expression(expr, self.unresolved_ctxt),
            },
            None => true,
          };

          if !pure_init {
            self.has_side_effects_stmt = true;
            return;
          }

          let pure_test = match stmt.test {
            Some(box ref test) => is_pure_expression(test, self.unresolved_ctxt),
            None => true,
          };

          if !pure_test {
            self.has_side_effects_stmt = true;
            return;
          }

          let pure_update = match stmt.update {
            Some(ref expr) => is_pure_expression(expr, self.unresolved_ctxt),
            None => true,
          };

          if !pure_update {
            self.has_side_effects_stmt = true;
          }
        }
        ModuleItem::Stmt(Stmt::Expr(stmt)) => {
          if !is_pure_expression(&stmt.expr, self.unresolved_ctxt) {
            self.has_side_effects_stmt = true;
          }
        }
        ModuleItem::Stmt(Stmt::Switch(stmt)) => {
          if !is_pure_expression(&stmt.discriminant, self.unresolved_ctxt) {
            self.has_side_effects_stmt = true;
          }
        }
        ModuleItem::Stmt(Stmt::Decl(stmt)) => {
          if !is_pure_decl(stmt, self.unresolved_ctxt) {
            self.has_side_effects_stmt = true;
          }
        }
        ModuleItem::ModuleDecl(module_decl) => match module_decl {
          ModuleDecl::ExportDecl(decl) => {
            if !is_pure_decl(&decl.decl, self.unresolved_ctxt) {
              self.has_side_effects_stmt = true;
            }
          }
          ModuleDecl::ExportDefaultDecl(decl) => {
            match decl.decl {
              DefaultDecl::Class(ref class) => {
                if !is_pure_class(&class.class, self.unresolved_ctxt) {
                  self.has_side_effects_stmt = true;
                }
              }
              DefaultDecl::Fn(_) => {}
              DefaultDecl::TsInterfaceDecl(_) => unreachable!(),
            };
          }
          ModuleDecl::ExportDefaultExpr(expr) => {
            if !is_pure_expression(&expr.expr, self.unresolved_ctxt) {
              self.has_side_effects_stmt = true;
            }
          }
          ModuleDecl::ExportAll(_)
          | ModuleDecl::Import(_)
          | ModuleDecl::ExportNamed(_)
          | ModuleDecl::TsImportEquals(_)
          | ModuleDecl::TsExportAssignment(_)
          | ModuleDecl::TsNamespaceExport(_) => {}
        },
        ModuleItem::Stmt(Stmt::Empty(_)) => {}
        ModuleItem::Stmt(Stmt::Labeled(_)) => {}
        ModuleItem::Stmt(Stmt::Block(_)) => {}
        _ => self.has_side_effects_stmt = true,
      };
    }
  }
  fn add_export(&mut self, id: JsWord, symbol: SymbolRef) {
    match self.export_map.entry(id) {
      Entry::Occupied(_) => {
        // TODO: should add some Diagnostic
      }
      Entry::Vacant(vac) => {
        vac.insert(symbol);
      }
    }
  }

  fn add_import(&mut self, id: BetterId, symbol: SymbolRef) {
    match self.import_map.entry(id) {
      Entry::Occupied(_) => {
        // TODO: should add some Diagnostic
      }
      Entry::Vacant(vac) => {
        // if import is a helper injection then we should ignore now tree-shaking with that module
        // one more thing, only helper module inserted by swc transformer will be ignored
        // e.g. import ext from '@swc/helper/xxx'
        if vac.key().ctxt.outer() == self.helper_mark {
          match self
            .bail_out_module_identifiers
            .entry(symbol.module_identifier())
          {
            Entry::Occupied(mut occ) => {
              *occ.get_mut() |= BailoutFlag::HELPER;
            }
            Entry::Vacant(vac) => {
              vac.insert(BailoutFlag::HELPER);
            }
          }
        }
        self.potential_top_mark.insert(vac.key().ctxt.outer());
        vac.insert(symbol);
      }
    }
  }
  fn analyze_named_export(&mut self, named_export: &NamedExport) {
    let src = named_export.src.as_ref().map(|src| &src.value);
    if let Some(src) = src {
      let resolved_uri = match self.resolve_module_identifier(src, &DependencyType::EsmExport) {
        Some(module_identifier) => module_identifier,
        None => {
          eprintln!(
            "Can't resolve import {} in module {} ",
            // SAFETY: we already know that src is not empty
            named_export.src.as_ref().expect("TODO:").value,
            self.module_identifier
          );
          // TODO: Ignore because helper interference.
          return;
        }
      };
      let resolved_uri_ukey = *resolved_uri;
      named_export
        .specifiers
        .iter()
        .for_each(|specifier| match specifier {
          ExportSpecifier::Namespace(namespace) => {
            let atom = match namespace.name {
              ModuleExportName::Ident(ref ident) => ident.sym.clone(),
              ModuleExportName::Str(ref str) => str.value.clone(),
            };
            self.add_export(
              atom.clone(),
              SymbolRef::Star(StarSymbol::new(
                resolved_uri_ukey,
                atom,
                self.module_identifier,
                StarSymbolKind::ReExportAllAs,
              )),
            );
          }
          ExportSpecifier::Default(_) => {
            // Currently swc does not support syntax like `export v from 'xxx';
            // Since this is a syntax error the logic should not reach here.`
            unreachable!("Module has syntax error should not trigger tree_shaking")
          }
          ExportSpecifier::Named(named) => {
            // TODO: what if the named binding is a unresolved_binding?
            // TODO: handle `as xxx`
            let original = match &named.orig {
              ModuleExportName::Ident(ident) => ident.sym.clone(),
              ModuleExportName::Str(str) => str.value.clone(),
            };
            let exported = named.exported.clone().map(|exported| match exported {
              ModuleExportName::Ident(ident) => ident.sym,
              ModuleExportName::Str(str) => str.value,
            });

            let exported_atom = exported.clone().unwrap_or_else(|| original.clone());

            self.reachable_import_and_export.insert(
              exported_atom.clone(),
              HashSet::from_iter([SymbolRef::Indirect(IndirectTopLevelSymbol::new(
                resolved_uri_ukey,
                self.module_identifier,
                IndirectType::Temp(original.clone()),
              ))]),
            );

            let exported_symbol = SymbolRef::Indirect(IndirectTopLevelSymbol::new(
              self.module_identifier,
              self.module_identifier,
              IndirectType::ReExport(original, exported),
            ));
            self.add_export(exported_atom, exported_symbol);
          }
        });
    } else {
      named_export
        .specifiers
        .iter()
        .for_each(|specifier| match specifier {
          ExportSpecifier::Namespace(_) => {
            // named_export has namespace specifier but no src will trigger a syntax error and should not reach here. e.g.
            // `export *`;
            unreachable!("")
          }
          ExportSpecifier::Default(_) => {
            // Currently swc does not support syntax like `export v from 'xxx';`
            unreachable!("Module has syntax error should not reach tree shaking analyze")
          }
          ExportSpecifier::Named(named) => {
            // TODO: what if the named binding is a unresolved_binding?
            let id: BetterId = match &named.orig {
              ModuleExportName::Ident(ident) => ident.to_id().into(),
              // `export {'a'}` is a syntax error;
              // we know here export has no src,  so this branch should not reachable.
              ModuleExportName::Str(_) => unreachable!(),
            };

            let exported_atom = match named.exported {
              Some(ref exported) => match exported {
                ModuleExportName::Ident(ident) => ident.sym.clone(),
                ModuleExportName::Str(str) => str.value.clone(),
              },
              None => id.atom.clone(),
            };
            let symbol_ref =
              SymbolRef::Direct(Symbol::new(self.module_identifier, id, SymbolType::Temp));

            self.add_export(exported_atom, symbol_ref);
          }
        });
    };
  }

  /// Try to get the module_identifier from `src`, `dependency_type`, and `importer`
  /// For simplicity, this function will assume the importer is always `self.module_identifier`
  /// # Panic
  /// This function will panic if can't find
  fn resolve_module_identifier(
    &self,
    src: &str,
    dependency_type: &DependencyType,
  ) -> Option<&ModuleIdentifier> {
    self
      .module_graph
      .module_graph_module_by_identifier(&self.module_identifier)
      .and_then(|mgm| {
        mgm.dependencies.iter().find_map(|id| {
          let dep = self
            .module_graph
            .dependency_by_id(id)
            .expect("should have dependency");
          if dep.request() == src && dependency_type == dep.dependency_type() {
            self
              .module_graph
              .module_graph_module_by_dependency_id(dep.id().expect("should have id"))
              .map(|module| &module.module_identifier)
          } else {
            None
          }
        })
      })
  }
}

/// The `allow(unused)` will be removed after the Tree shaking is finished
#[derive(Debug)]
#[allow(unused)]
pub struct TreeShakingResult {
  top_level_mark: Mark,
  unresolved_mark: Mark,
  pub module_identifier: ModuleIdentifier,
  pub export_map: HashMap<JsWord, SymbolRef>,
  pub(crate) import_map: HashMap<BetterId, SymbolRef>,
  pub inherit_export_maps: IdentifierLinkedMap<HashMap<JsWord, SymbolRef>>,
  // current_region: Option<BetterId>,
  // pub(crate) reference_map: HashMap<BetterId, HashSet<BetterId>>,
  pub(crate) reachable_import_of_export: HashMap<JsWord, HashSet<SymbolRef>>,
  state: AnalyzeState,
  pub(crate) used_symbol_refs: HashSet<SymbolRef>,
  pub(crate) bail_out_module_identifiers: IdentifierMap<BailoutFlag>,
  pub(crate) side_effects: SideEffect,
}

impl From<ModuleRefAnalyze<'_>> for TreeShakingResult {
  fn from(mut analyze: ModuleRefAnalyze<'_>) -> Self {
    Self {
      top_level_mark: std::mem::take(&mut analyze.top_level_mark),
      unresolved_mark: std::mem::take(&mut analyze.unresolved_mark),
      module_identifier: std::mem::take(&mut analyze.module_identifier),
      export_map: std::mem::take(&mut analyze.export_map),
      import_map: std::mem::take(&mut analyze.import_map),
      inherit_export_maps: std::mem::take(&mut analyze.inherit_export_maps),
      // current_region: std::mem::take(&mut analyze.current_body_owner_id),
      // reference_map: std::mem::take(&mut analyze.reference_map),
      reachable_import_of_export: std::mem::take(&mut analyze.reachable_import_and_export),
      state: std::mem::take(&mut analyze.state),
      used_symbol_refs: std::mem::take(&mut analyze.used_symbol_ref),
      bail_out_module_identifiers: std::mem::take(&mut analyze.bail_out_module_identifiers),
      side_effects: analyze.side_effects,
    }
  }
}

fn first_ident_of_assign_lhs(node: &AssignExpr) -> Option<Id> {
  let mut visitor = FirstIdentVisitor::default();
  node.left.visit_with(&mut visitor);
  visitor.id
}

#[derive(Default)]
struct FirstIdentVisitor {
  id: Option<Id>,
}

impl Visit for FirstIdentVisitor {
  noop_visit_type!();

  fn visit_ident(&mut self, node: &Ident) {
    if self.id.is_none() {
      self.id = Some(node.to_id());
    }
  }
}

fn is_pure_expression(expr: &Expr, unresolved_ctxt: SyntaxContext) -> bool {
  match expr {
    // Mark `module.exports = require('xxx')` as pure
    Expr::Assign(AssignExpr {
      left: PatOrExpr::Expr(box left_expr),
      right: box Expr::Call(call_expr_right),
      op: op!("="),
      ..
    }) if is_module_exports_member_expr(left_expr, unresolved_ctxt)
      && get_require_literal(call_expr_right, unresolved_ctxt.outer()).is_some() =>
    {
      true
    }
    _ => !expr.may_have_side_effects(&ExprCtx {
      unresolved_ctxt,
      is_unresolved_ref_safe: false,
    }),
  }
}

/// Check if the expression is `module.exports`
fn is_module_exports_member_expr(expr: &Expr, unresolved_ctxt: SyntaxContext) -> bool {
  matches!(expr, Expr::Member(MemberExpr {
    obj:
      box Expr::Ident(Ident {
        sym: js_word!("module"),
        span: obj_span,
        ..
      }),
    prop: MemberProp::Ident(Ident { sym: prop_sym, .. }),
    ..
  }) if obj_span.ctxt == unresolved_ctxt && prop_sym == "exports")
}

fn is_pure_decl(stmt: &Decl, unresolved_ctxt: SyntaxContext) -> bool {
  match stmt {
    Decl::Class(class) => is_pure_class(&class.class, unresolved_ctxt),
    Decl::Fn(_) => true,
    Decl::Var(var) => is_pure_var_decl(var, unresolved_ctxt),
    Decl::TsInterface(_) => unreachable!(),
    Decl::TsTypeAlias(_) => unreachable!(),

    Decl::TsEnum(_) => unreachable!(),
    Decl::TsModule(_) => unreachable!(),
  }
}

fn is_pure_class(class: &Class, unresolved_ctxt: SyntaxContext) -> bool {
  if let Some(ref super_class) = class.super_class {
    if !is_pure_expression(super_class, unresolved_ctxt) {
      return false;
    }
  }
  let is_pure_key = |key: &PropName| -> bool {
    match key {
      PropName::BigInt(_) | PropName::Ident(_) | PropName::Str(_) | PropName::Num(_) => true,
      PropName::Computed(ref computed) => is_pure_expression(&computed.expr, unresolved_ctxt),
    }
  };

  class.body.iter().all(|item| -> bool {
    match item {
      ClassMember::Constructor(cons) => is_pure_key(&cons.key),
      ClassMember::Method(method) => is_pure_key(&method.key),
      ClassMember::PrivateMethod(method) => {
        is_pure_expression(&Expr::PrivateName(method.key.clone()), unresolved_ctxt)
      }
      ClassMember::ClassProp(prop) => {
        is_pure_key(&prop.key)
          && (!prop.is_static
            || if let Some(ref value) = prop.value {
              is_pure_expression(value, unresolved_ctxt)
            } else {
              true
            })
      }
      ClassMember::PrivateProp(prop) => {
        is_pure_expression(&Expr::PrivateName(prop.key.clone()), unresolved_ctxt)
          && (!prop.is_static
            || if let Some(ref value) = prop.value {
              is_pure_expression(value, unresolved_ctxt)
            } else {
              true
            })
      }
      ClassMember::TsIndexSignature(_) => unreachable!(),
      ClassMember::Empty(_) => true,
      ClassMember::StaticBlock(_) => true,
    }
  })
}

fn is_pure_var_decl(var: &VarDecl, unresolved_ctxt: SyntaxContext) -> bool {
  var.decls.iter().all(|decl| {
    if let Some(ref init) = decl.init {
      is_pure_expression(init, unresolved_ctxt)
    } else {
      true
    }
  })
}

fn is_import_decl(module_item: &ModuleItem) -> bool {
  matches!(module_item, ModuleItem::ModuleDecl(ModuleDecl::Import(_)))
}

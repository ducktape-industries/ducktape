// Guard the load-bearing policy shape with the TypeScript parser: one pure
// domain dispatch, no local board/worker runtime, and no model-context snapshots.
import { strict as assert } from 'node:assert';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import ts from 'typescript';

// -- Parsed source helpers ---------------------------------------------------
const source = (name: string): Promise<ts.SourceFile> => Promise.resolve()
  .then(() => readFile(new URL(name, import.meta.url), 'utf8'))
  .then(text => ts.createSourceFile(name, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS));
const nodes = (node: ts.Node): ts.Node[] => [node, ...node.getChildren().flatMap(nodes)];
const variable = (file: ts.SourceFile, name: string): ts.VariableDeclaration => {
  const declaration = nodes(file).filter(ts.isVariableDeclaration)
    .find(item => ts.isIdentifier(item.name) && item.name.text === name);
  assert(declaration, `Missing ${name}`);
  return declaration;
};
const member = (object: ts.ObjectLiteralExpression, name: string): ts.PropertyAssignment => {
  const property = object.properties.find(item => ts.isPropertyAssignment(item) && item.name.getText() === name);
  assert(property && ts.isPropertyAssignment(property), `Missing ${name}`);
  return property;
};
const propertyCall = (call: ts.CallExpression, owner: string, name: string): boolean => ts.isPropertyAccessExpression(call.expression)
  && call.expression.expression.getText() === owner && call.expression.name.text === name;

// -- Domain steps cannot hide effects or inlined routing logic ---------------
test('domain decide is one exhaustive dispatch with only named handler delegations', async () => {
  const [domain, contracts] = await Promise.all([source('domain.ts'), source('contracts.ts')]);
  const initializer = variable(domain, 'decide').initializer;
  assert(initializer && ts.isArrowFunction(initializer) && ts.isBlock(initializer.body));
  assert.equal(initializer.body.statements.length, 1);
  const dispatch = initializer.body.statements[0];
  assert(ts.isSwitchStatement(dispatch));
  assert.equal(dispatch.expression.getText(), 'action.kind');
  const defaults = dispatch.caseBlock.clauses.filter(ts.isDefaultClause);
  assert.equal(defaults.length, 1);
  dispatch.caseBlock.clauses.forEach(clause => {
    assert.equal(clause.statements.length, 1);
    const statement = clause.statements[0];
    assert(ts.isReturnStatement(statement) && statement.expression && ts.isCallExpression(statement.expression));
    const call = statement.expression;
    assert(ts.isIdentifier(call.expression), 'Dispatch arms delegate to a named pure handler');
    if (ts.isDefaultClause(clause)) {
      assert.equal(call.expression.text, 'assertNever');
      assert.deepEqual(call.arguments.map(argument => argument.getText()), ['action']);
      return;
    }
    assert.deepEqual(call.arguments.map(argument => argument.getText()), ['board', 'action']);
    assert(variable(domain, call.expression.text).initializer);
  });
  const action = contracts.statements.find(item => ts.isTypeAliasDeclaration(item) && item.name.text === 'DomainAction');
  assert(action && ts.isTypeAliasDeclaration(action) && ts.isUnionTypeNode(action.type));
  const variants = action.type.types.map(type => {
    assert(ts.isTypeLiteralNode(type));
    const kind = type.members.find(item => ts.isPropertySignature(item) && item.name.getText() === 'kind');
    assert(kind && ts.isPropertySignature(kind) && kind.type && ts.isLiteralTypeNode(kind.type) && ts.isStringLiteral(kind.type.literal));
    return kind.type.literal.text;
  });
  const routed = dispatch.caseBlock.clauses.filter(ts.isCaseClause).map(clause => {
    assert(ts.isStringLiteral(clause.expression));
    return clause.expression.text;
  });
  assert.deepEqual(routed.sort(), variants.sort());
  nodes(domain).filter(ts.isImportDeclaration).forEach(declaration => assert(declaration.importClause?.isTypeOnly, 'The domain may import types, not effectful runtimes'));
  const effects = new Set(['fetch', 'process', 'console', 'require', 'eval', 'Function', 'WebSocket', 'XMLHttpRequest', 'setTimeout', 'setInterval']);
  nodes(domain).forEach(node => {
    assert(!ts.isAwaitExpression(node), 'Pure domain steps never await effects');
    if (ts.isIdentifier(node)) assert(!effects.has(node.text), `Effectful domain reference ${node.text}`);
    if (ts.isPropertyAccessExpression(node)) assert(!['Date.now', 'Math.random'].includes(node.getText()), 'Pure steps cannot read time or randomness');
  });
});

// -- Runtime boundaries stay generic and network-owned -----------------------
test('product sources have no local board store, native worker IPC or hidden session storage', async () => {
  const files = await Promise.all(['index.ts', 'tools.ts', 'prompt.ts', 'contracts.ts', 'domain.ts', 'service.ts', 'store.ts', 'views.ts', 'network.ts', 'network-pages.ts', 'inputs.ts'].map(source));
  const forbiddenImports = /^(?:node:)?(?:child_process|net|http|https|tls|dgram|worker_threads)$|^(?:ws|undici|axios|better-sqlite3)$/;
  const forbiddenNames = new Set(['createAgentSession', 'SessionManager', 'sessionManager', 'sendUserMessage', 'appendEntry', 'setInterval', 'setTimeout', 'spawn', 'fork', 'execFile', 'execSync', 'WebSocket']);
  files.forEach(file => {
    const all = nodes(file);
    all.filter(ts.isImportDeclaration).forEach(declaration => {
      assert(ts.isStringLiteral(declaration.moduleSpecifier));
      const specifier = declaration.moduleSpecifier.text;
      assert(!forbiddenImports.test(specifier), `${file.fileName} imports a native transport/worker runtime`);
      const filesystem = /^(?:node:)?fs(?:\/promises)?$/.test(specifier);
      if (!filesystem) return;
      assert.equal(file.fileName, 'network.ts', 'Only immutable per-install network configuration may read the filesystem');
      const bindings = declaration.importClause?.namedBindings;
      assert(bindings && ts.isNamedImports(bindings));
      assert.deepEqual(bindings.elements.map(item => item.name.text), ['readFile']);
    });
    all.filter(ts.isIdentifier).forEach(identifier => assert(!forbiddenNames.has(identifier.text), `${file.fileName} uses forbidden runtime ${identifier.text}`));
    all.filter(ts.isCallExpression).forEach(call => {
      assert(call.expression.kind !== ts.SyntaxKind.ImportKeyword, 'Dynamic imports may not hide a worker transport');
      const isRequire = ts.isIdentifier(call.expression) && call.expression.text === 'require';
      assert(!isRequire, 'CommonJS require may not bypass import ownership checks');
      const isReadFile = ts.isIdentifier(call.expression) && call.expression.text === 'readFile';
      if (!isReadFile) return;
      const location = call.arguments[0];
      assert(ts.isNewExpression(location) && location.expression.getText() === 'URL');
      assert.equal(location.arguments?.[0].getText(), "'./chief.config.json'");
    });
  });
});

test('Pi hooks inject only static policy and next-turn control identities, never board snapshots', async () => {
  const index = await source('index.ts');
  const calls = nodes(index).filter(ts.isCallExpression);
  const sends = calls.filter(call => propertyCall(call, 'pi', 'sendMessage'));
  assert.equal(sends.length, 1, 'Only appendControl may append a model-visible message');
  const [message, options] = sends[0].arguments;
  assert(ts.isObjectLiteralExpression(message) && ts.isObjectLiteralExpression(options));
  assert.equal(member(message, 'customType').initializer.getText(), "'chief-control'");
  assert.equal(member(options, 'triggerTurn').initializer.kind, ts.SyntaxKind.FalseKeyword);
  assert.equal(member(options, 'deliverAs').initializer.getText(), "'nextTurn'");
  const hooks = calls.filter(call => propertyCall(call, 'pi', 'on'));
  hooks.forEach(hook => {
    const event = hook.arguments[0];
    assert(ts.isStringLiteral(event));
    assert(!['context', 'before_provider_request', 'session_before_compact'].includes(event.text), 'Board state must not be rewritten into model history');
    const lifecycle = ['session_start', 'before_agent_start', 'session_shutdown'].includes(event.text);
    if (!lifecycle) return;
    nodes(hook.arguments[1]).filter(ts.isCallExpression).forEach(call => {
      const name = ts.isPropertyAccessExpression(call.expression) ? call.expression.name.text : call.expression.getText();
      assert(!['read', 'execute', 'boardView', 'workerBrief'].includes(name), 'Lifecycle hooks cannot automatically pull board state');
    });
  });
  const prompts = nodes(index).filter(ts.isPropertyAssignment).filter(property => property.name.getText() === 'systemPrompt');
  assert.equal(prompts.length, 1);
  assert(ts.isTemplateExpression(prompts[0].initializer));
  const spans = prompts[0].initializer.templateSpans;
  assert.equal(spans.length, 2);
  assert(ts.isPropertyAccessExpression(spans[0].expression) && spans[0].expression.name.text === 'systemPrompt');
  assert.equal(spans[1].expression.getText(), 'CHIEF_PROMPT');
});

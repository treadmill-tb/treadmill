import { $api } from "../api/client";

/** A JSON Schema node, walked structurally rather than typed field by field. */
type Schema = Record<string, unknown>;

function isSchema(value: unknown): value is Schema {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Follow a `$ref` into the document's `$defs`. */
function deref(schema: Schema | undefined, root: Schema): Schema | undefined {
  if (schema === undefined) return undefined;
  const ref = schema["$ref"];
  if (typeof ref !== "string") return schema;
  const name = ref.replace("#/$defs/", "");
  const defs = root["$defs"];
  if (!isSchema(defs)) return undefined;
  const target = defs[name];
  return isSchema(target) ? deref(target, root) : undefined;
}

/**
 * Narrow a schema to the branch describing `value`.
 *
 * `anyOf` with a single branch is how the versioned spec enum is emitted;
 * `oneOf` with a `const` discriminant is how an internally-tagged enum
 * (`Platform`, `Console`) is. Picking the matching branch is what lets a
 * variant's own field docs reach the page.
 */
function forValue(
  schema: Schema | undefined,
  value: unknown,
  root: Schema,
): Schema | undefined {
  const resolved = deref(schema, root);
  if (resolved === undefined) return undefined;
  const branches = resolved["anyOf"] ?? resolved["oneOf"];
  if (!Array.isArray(branches)) return resolved;

  const candidates = branches
    .filter(isSchema)
    .map((b) => deref(b, root))
    .filter((b): b is Schema => b !== undefined);
  if (candidates.length === 1) return candidates[0];
  if (!isSchema(value)) return candidates[0];

  const matching = candidates.find((branch) => {
    const properties = branch["properties"];
    if (!isSchema(properties)) return false;
    return Object.entries(properties).some(
      ([key, sub]) => isSchema(sub) && sub["const"] === value[key],
    );
  });
  return matching ?? candidates[0];
}

function propertyOf(
  schema: Schema | undefined,
  key: string,
  root: Schema,
): Schema | undefined {
  const properties = deref(schema, root)?.["properties"];
  if (!isSchema(properties)) return undefined;
  const sub = properties[key];
  return isSchema(sub) ? sub : undefined;
}

function itemsOf(schema: Schema | undefined, root: Schema): Schema | undefined {
  const items = deref(schema, root)?.["items"];
  return isSchema(items) ? items : undefined;
}

/**
 * The rustdoc schemars lifted into `description`, as a one-line tooltip.
 *
 * The Rust type is the single source for the validator, the CEL environment
 * and this copy, so the labels here are never a second description to keep in
 * step with the first.
 */
function help(schema: Schema | undefined): string | undefined {
  const description = schema?.["description"];
  return typeof description === "string" ? description : undefined;
}

function Scalar({ value }: { value: unknown }) {
  if (value === null || value === undefined) {
    return <span className="muted">—</span>;
  }
  if (typeof value === "boolean") {
    return <span>{value ? "yes" : "no"}</span>;
  }
  return <span className="mono">{String(value)}</span>;
}

function Value({
  value,
  schema,
  root,
}: {
  value: unknown;
  schema: Schema | undefined;
  root: Schema;
}) {
  if (value === null || value === undefined) return <Scalar value={value} />;

  if (Array.isArray(value)) {
    if (value.length === 0) return <span className="muted">none</span>;
    const items = itemsOf(schema, root);
    if (value.every((v) => !isSchema(v))) {
      return (
        <>
          {value.map((v, i) => (
            <span key={i} className="chip mono">
              {String(v)}
            </span>
          ))}
        </>
      );
    }
    return (
      <>
        {value.map((v, i) => (
          <div key={i} className="spec-item">
            <Fields value={v} schema={forValue(items, v, root)} root={root} />
          </div>
        ))}
      </>
    );
  }

  if (isSchema(value)) {
    return (
      <Fields
        value={value}
        schema={forValue(schema, value, root)}
        root={root}
      />
    );
  }

  return <Scalar value={value} />;
}

/** One object's fields as a definition list, keyed by the wire field names. */
function Fields({
  value,
  schema,
  root,
}: {
  value: unknown;
  schema: Schema | undefined;
  root: Schema;
}) {
  if (!isSchema(value)) return <Scalar value={value} />;
  const entries = Object.entries(value);
  if (entries.length === 0) return <span className="muted">none</span>;

  return (
    <dl className="props spec-fields">
      {entries.map(([key, sub]) => {
        const subSchema = propertyOf(schema, key, root);
        const title = help(subSchema);
        return (
          <div key={key} className="spec-field">
            {/* The raw field name, not a prettified one: it is what a CEL
                predicate spells, so showing anything else would mislead. */}
            <dt className="mono" title={title}>
              {key}
              {title !== undefined && <span className="spec-hint">?</span>}
            </dt>
            <dd>
              <Value value={sub} schema={subSchema} root={root} />
            </dd>
          </div>
        );
      })}
    </dl>
  );
}

/**
 * Render a host spec against the schema the switchboard publishes.
 *
 * The schema supplies the field documentation; without it the values still
 * render, just without help text, so a fetch failure degrades rather than
 * blanking the page.
 */
export function HostSpecView({ spec }: { spec: unknown }) {
  const schema = $api.useQuery("get", "/host-spec/schema");
  const root: Schema = isSchema(schema.data) ? schema.data : {};
  return (
    <Fields value={spec} schema={forValue(root, spec, root)} root={root} />
  );
}

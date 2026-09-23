// Well-known subjects (see `SCHEMA.sql`).
export const ADMINS_GROUP = "00000000-0000-0000-0000-000000000001";
/** Owner of the standard images. */
export const SYSTEM_SUBJECT = "00000000-0000-0000-0000-000000000002";
/** Implicitly contains every subject; granting it makes things public. */
export const EVERYONE_SUBJECT = "00000000-0000-0000-0000-000000000004";

const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isUuid(value: string): boolean {
  return UUID_RE.test(value);
}

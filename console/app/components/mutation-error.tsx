import { describeError, type ErrorMessages } from "../api/errors";

/** A failed request's error as a sentence, worded by `messages` for the
 * statuses whose meaning depends on the call. */
export function MutationError({
  error,
  messages,
}: {
  error: unknown;
  messages?: ErrorMessages;
}) {
  if (error == null) {
    return null;
  }
  return <p className="error">{describeError(error, messages)}</p>;
}

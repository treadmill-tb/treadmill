/** Render a mutation's thrown error body (string, object, or Error). */
export function MutationError({ error }: { error: unknown }) {
  if (error == null) {
    return null;
  }
  let text: string;
  if (typeof error === "string") {
    text = error;
  } else if (error instanceof Error) {
    text = error.message;
  } else {
    text = JSON.stringify(error);
  }
  return <p className="error">{text === "{}" ? "Request failed." : text}</p>;
}

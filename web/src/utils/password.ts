/** Mirrors `MIN_PASSWORD_LEN` in `src/routes/auth.rs`. */
export const MIN_PASSWORD_LEN = 12

/**
 * The unmet password requirement, or `''` when the password is acceptable.
 *
 * Callers render this against the password field — never merged into a
 * form-wide message that also names fields the user got right.
 */
export function passwordProblem(password: string): string {
  if (password.length < MIN_PASSWORD_LEN) {
    return `Password must be at least ${MIN_PASSWORD_LEN} characters`
  }
  return ''
}

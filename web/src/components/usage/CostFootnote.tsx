/** The standing caveat under every set of dollar figures on the usage pages:
 *  they are estimates, they are USD, and they come from one named rate table.
 *  A bare `$` next to a token count reads as an invoice; naming the source
 *  makes it checkable — `GET /api/usage/costs` is the exact table the backend
 *  priced `est_cost` with (`src/routes/usage/cost.rs`).
 *
 *  One component rather than three copies of the sentence, so the wording
 *  cannot drift between the overview, the session page and the project page. */
export default function CostFootnote() {
  return (
    <p className="usage-footnote" data-testid="usage-cost-footnote">
      Costs are estimates in US dollars, priced from the per-model rate table this build serves at{' '}
      <code>/api/usage/costs</code>. They are not billing figures.
    </p>
  )
}

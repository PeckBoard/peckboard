import rehypeHighlight from 'rehype-highlight'

// The theme that colours the `hljs-*` spans the plugin emits. It lived in
// ChatView, so every other markdown surface got highlighted code only as long
// as the chat happened to be in the module graph. Importing it here ties the
// stylesheet to the plugin itself.
import 'highlight.js/styles/github-dark.css'

/** The rehype plugins every markdown surface uses for fenced code.
 *
 *  Module scope, so the array identity is stable: a fresh literal re-runs the
 *  whole markdown pipeline on each render of the surface that owns it. */
export const highlightPlugins = [rehypeHighlight]

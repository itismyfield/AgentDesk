import tlc2.tool.impl.FastTool;
import tlc2.tool.impl.SpecProcessor;

/** Inspect the selected specification using the same parser as the TLC checker. */
class StatechartFairness {
    public static void main(String[] args) {
        if (args.length != 2) {
            System.err.println("Expected module and config arguments");
            System.exit(2);
        }
        try {
            String config = args[1].endsWith(".cfg") ? args[1].substring(0, args[1].length() - 4) : args[1];
            SpecProcessor spec = new FastTool(args[0], config).getSpecProcessor();
            int assumptions = spec.getTemporal().length;
            int properties = spec.getImpliedTemporals().length;
            if (assumptions == 0) {
                System.err.println("liveness SPECIFICATION has no applied fairness assumptions");
                System.exit(2);
            }
            if (properties == 0) {
                System.err.println("liveness PROPERTY has no temporal formula after TLC processing");
                System.exit(2);
            }
            System.out.println("STATECHART_FAIRNESS_OK assumptions=" + assumptions + " properties=" + properties);
        } catch (RuntimeException error) {
            System.err.println("Cannot inspect liveness SPECIFICATION: " + error.getMessage());
            System.exit(2);
        }
    }
}

package dev.henka.jdtls;

import java.util.List;

import com.google.gson.Gson;
import com.google.gson.JsonElement;

import org.eclipse.core.runtime.IProgressMonitor;
import org.eclipse.jdt.ls.core.internal.IDelegateCommandHandler;
import org.eclipse.jdt.ls.core.internal.JSONUtility;
import org.eclipse.jdt.ls.core.internal.handlers.ChangeSignatureInfoHandler;
import org.eclipse.jdt.ls.core.internal.handlers.GetRefactorEditHandler;
import org.eclipse.jdt.ls.core.internal.handlers.GetRefactorEditHandler.GetRefactorEditParams;
import org.eclipse.jdt.ls.core.internal.handlers.MoveHandler;
import org.eclipse.jdt.ls.core.internal.handlers.MoveHandler.MoveParams;
import org.eclipse.lsp4j.CodeActionParams;

/**
 * Exposes Eclipse JDT LS refactoring handlers that the stock distribution ships
 * but does not register as executeCommand delegates. Loaded into jdtls via the
 * {@code bundles} initialization option so a headless client (the Henka MCP
 * server) can drive change-signature, move, and parameterized extract.
 */
public class HenkaDelegateCommandHandler implements IDelegateCommandHandler {

    public static final String GET_REFACTOR_EDIT = "henka.mcp.getRefactorEdit";
    public static final String GET_MOVE_DESTINATIONS = "henka.mcp.getMoveDestinations";
    public static final String MOVE = "henka.mcp.move";
    public static final String GET_CHANGE_SIGNATURE_INFO = "henka.mcp.getChangeSignatureInfo";

    private static final Gson GSON = new Gson();

    @Override
    public Object executeCommand(String commandId, List<Object> arguments, IProgressMonitor monitor)
            throws Exception {
        try {
            switch (commandId) {
                case GET_REFACTOR_EDIT: {
                    GetRefactorEditParams params = model(arguments, 0, GetRefactorEditParams.class);
                    // Some refactorings (change-signature) format generated code and
                    // require options; supply a default so callers needn't serialize
                    // lsp4j FormattingOptions over the wire.
                    if (params.options == null) {
                        params.options = new org.eclipse.lsp4j.FormattingOptions(4, true);
                    }
                    // The handler re-models each commandArgument with JSONUtility,
                    // which only understands JsonElement (it returns null for a raw
                    // map/list). Re-wrap them as JsonElements so e.g. the
                    // change-signature parameter array deserializes instead of NPEing.
                    if (params.commandArguments != null) {
                        java.util.List<Object> wrapped =
                                new java.util.ArrayList<>(params.commandArguments.size());
                        for (Object a : params.commandArguments) {
                            wrapped.add(a instanceof JsonElement ? a : GSON.toJsonTree(a));
                        }
                        params.commandArguments = wrapped;
                    }
                    return GetRefactorEditHandler.getEditsForRefactor(params);
                }
                case GET_MOVE_DESTINATIONS:
                    return MoveHandler.getMoveDestinations(model(arguments, 0, MoveParams.class));
                case MOVE:
                    return MoveHandler.move(model(arguments, 0, MoveParams.class), monitor);
                case GET_CHANGE_SIGNATURE_INFO:
                    return ChangeSignatureInfoHandler.getChangeSignatureInfo(model(arguments, 0, CodeActionParams.class), monitor);
                default:
                    throw new UnsupportedOperationException("Unknown command: " + commandId);
            }
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder(commandId).append(": ")
                    .append(t.getClass().getName()).append(": ").append(t.getMessage());
            for (StackTraceElement f : t.getStackTrace()) {
                if (f.getClassName().startsWith("org.eclipse.jdt")) {
                    sb.append(" @ ").append(f);
                    break;
                }
            }
            throw new RuntimeException(sb.toString(), t);
        }
    }

    /**
     * Deserialize argument {@code index} into {@code clazz}. The argument arrives
     * from the LSP layer as a gson {@code LinkedTreeMap}; we convert it to a
     * {@code JsonElement} so jdtls's lsp4j-aware gson can model it (its
     * {@code toModel} returns null for a raw map).
     */
    private static <T> T model(List<Object> arguments, int index, Class<T> clazz) {
        if (arguments == null || arguments.size() <= index || arguments.get(index) == null) {
            throw new IllegalArgumentException("missing argument " + index + " for refactoring command");
        }
        Object arg = arguments.get(index);
        JsonElement element = (arg instanceof JsonElement) ? (JsonElement) arg : GSON.toJsonTree(arg);
        return JSONUtility.toModel(element, clazz);
    }
}

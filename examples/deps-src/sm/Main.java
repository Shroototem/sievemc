package sm;
import net.fabricmc.api.*;
public class Main {
  public static void init() {
    net.minecraft.core.Registry.register(null, "servermod:block", new Object()); // server work
    cfg.ConfigLib.register();
    if (Boolean.getBoolean("othermod.loaded")) othermod.Api.hook(); // optional compat
  }
  @Environment(EnvType.CLIENT)
  public static void openScreen() { Runnable r = () -> ui.UiLib.open(); r.run(); }
}

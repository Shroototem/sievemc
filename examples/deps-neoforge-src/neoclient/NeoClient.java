package neoclient;
import net.neoforged.api.distmarker.Dist;
@net.neoforged.fml.common.Mod(value = "neo_clientdist", dist = Dist.CLIENT)
public class NeoClient { public NeoClient() { new com.mojang.brigadier.CommandDispatcher<Object>(); } }

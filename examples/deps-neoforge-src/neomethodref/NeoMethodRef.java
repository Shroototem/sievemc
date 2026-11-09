package neomethodref;
/** Its server work is only reachable through a method reference (Setup::init). */
@net.neoforged.fml.common.Mod("neo_methodref")
public class NeoMethodRef { public NeoMethodRef() { Runner.run(Setup::init); } }
